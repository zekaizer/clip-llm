mod overlay;
pub mod state_machine;

use std::sync::mpsc;
use tokio::sync::mpsc as tokio_mpsc;

use eframe::egui;
use tracing::{error, info};

use crate::clipboard::ClipboardManager;
use crate::hotkey::{TapAction, TapEvent};
use crate::platform::{NativePlatform, Platform};
use crate::worker::{ProcessTask, WorkerCommand, WorkerResponse};

pub use state_machine::OverlayState;
use state_machine::{StateMachine, UiEffect, UiEvent};

/// Polling interval for diagnostics scenario runner.
#[cfg(feature = "diagnostics")]
const IDLE_POLL_MS: u64 = 100;

pub struct OverlayApp {
    sm: StateMachine,
    cmd_tx: tokio_mpsc::UnboundedSender<WorkerCommand>,
    resp_rx: mpsc::Receiver<WorkerResponse>,
    clipboard: ClipboardManager,
    platform: NativePlatform,
    /// Mouse cursor position captured at hotkey trigger time.
    spawn_position: Option<egui::Pos2>,
    /// Whether the initial Visible(false) command has been sent at startup.
    initial_hide_done: bool,
    /// Tap events from coordinator thread (hotkey detection runs off-UI).
    tap_rx: mpsc::Receiver<TapEvent>,
    /// Cached desired_size to avoid redundant send_viewport_cmd calls.
    last_desired_size: Option<egui::Vec2>,
    /// Whether the think block section is expanded in the Result state.
    think_expanded: bool,
    #[cfg(feature = "diagnostics")]
    diag: crate::diagnostics::DiagCollector,
    #[cfg(feature = "diagnostics")]
    diag_action_rx: mpsc::Receiver<crate::diagnostics::ScenarioAction>,
    #[cfg(feature = "diagnostics")]
    diag_state_tx: mpsc::Sender<&'static str>,
    #[cfg(feature = "diagnostics")]
    prev_state_name: &'static str,
}

#[cfg(feature = "diagnostics")]
impl OverlayApp {
    pub fn new(
        cmd_tx: tokio_mpsc::UnboundedSender<WorkerCommand>,
        resp_rx: mpsc::Receiver<WorkerResponse>,
        clipboard: ClipboardManager,
        tap_rx: mpsc::Receiver<TapEvent>,
        diag_action_rx: mpsc::Receiver<crate::diagnostics::ScenarioAction>,
        diag_state_tx: mpsc::Sender<&'static str>,
    ) -> Self {
        Self {
            sm: StateMachine::new(crate::ProcessMode::default()),
            cmd_tx,
            resp_rx,
            clipboard,
            platform: NativePlatform,
            spawn_position: None,
            initial_hide_done: false,
            tap_rx,
            last_desired_size: None,
            think_expanded: false,
            diag: crate::diagnostics::DiagCollector::new(),
            diag_action_rx,
            diag_state_tx,
            prev_state_name: "Hidden",
        }
    }
}

#[cfg(not(feature = "diagnostics"))]
impl OverlayApp {
    pub fn new(
        cmd_tx: tokio_mpsc::UnboundedSender<WorkerCommand>,
        resp_rx: mpsc::Receiver<WorkerResponse>,
        clipboard: ClipboardManager,
        tap_rx: mpsc::Receiver<TapEvent>,
    ) -> Self {
        Self {
            sm: StateMachine::new(crate::ProcessMode::default()),
            cmd_tx,
            resp_rx,
            clipboard,
            platform: NativePlatform,
            spawn_position: None,
            initial_hide_done: false,
            tap_rx,
            last_desired_size: None,
            think_expanded: false,
        }
    }
}

impl OverlayApp {

    // -- Effect execution --

    fn execute_effects(&mut self, effects: Vec<UiEffect>, ctx: &egui::Context) {
        for effect in effects {
            match effect {
                UiEffect::SendProcess {
                    content,
                    mode,
                    rephrase_params,
                    thinking_mode,
                    request_id,
                } => {
                    let text_len = content.text.as_ref().map_or(0, |t| t.len());
                    let img_count = content.images.len();
                    info!("starting {} ({} chars, {} images)", mode.label(), text_len, img_count);
                    let _ = self.cmd_tx.send(WorkerCommand::Process(ProcessTask {
                        content,
                        mode,
                        rephrase_params,
                        thinking_mode,
                        request_id,
                    }));
                }
                UiEffect::SendCancel => {
                    let _ = self.cmd_tx.send(WorkerCommand::Cancel);
                }
                UiEffect::WriteClipboard(text) => {
                    if let Err(e) = self.clipboard.write_text(&text) {
                        error!("clipboard write failed: {e}");
                        let err_effects =
                            self.sm.handle(UiEvent::ClipboardError(e.to_string()));
                        // ClipboardError never emits WriteClipboard — recursion safe.
                        self.execute_effects(err_effects, ctx);
                    } else {
                        info!(
                            "{} complete ({} chars), copied to clipboard",
                            self.sm.mode().label(),
                            text.len()
                        );
                    }
                }
                UiEffect::ShowWindow => self.show_window(ctx),
                UiEffect::HideWindow => {
                    ctx.memory_mut(|m| m.reset_areas());
                    self.hide_window(ctx);
                    self.spawn_position = None;
                }
                UiEffect::CaptureMousePosition => self.capture_mouse_position(),
                UiEffect::ResetAreas => {
                    #[cfg(feature = "diagnostics")]
                    {
                        let to = self.sm.variant_name();
                        self.diag
                            .on_state_transition(self.prev_state_name, to);
                        self.prev_state_name = to;
                        // Notify scenario runner thread of state change.
                        let _ = self.diag_state_tx.send(to);
                    }
                    self.think_expanded = false;
                    ctx.memory_mut(|m| m.reset_areas());
                }
            }
        }
    }

    // -- Tap action handling (from coordinator thread) --

    fn poll_tap_actions(&mut self, ctx: &egui::Context) {
        while let Ok(tap_event) = self.tap_rx.try_recv() {
            // Set spawn_position from coordinator's first-press capture.
            // This runs before sm.handle() so CaptureMousePosition effect
            // (which skips if already set) preserves the first-press position.
            if let Some((x, y)) = tap_event.mouse_pos {
                self.spawn_position = Some(egui::pos2(x as f32, y as f32));
            }

            match tap_event.action {
                TapAction::SingleTap => {
                    info!("single-tap triggered, using clipboard content...");
                    let event = match self.clipboard.read_content() {
                        Ok(content) => UiEvent::ContentReady { content, auto_copy: false },
                        Err(e) => UiEvent::ClipboardError(e.to_string()),
                    };
                    let effects = self.sm.handle(event);
                    self.execute_effects(effects, ctx);
                }
                TapAction::DoubleTap => {
                    info!("double-tap triggered, copying selection...");
                    let event = match self.clipboard.copy_and_read(&self.platform) {
                        Ok(content) => UiEvent::ContentReady { content, auto_copy: true },
                        Err(e) => UiEvent::ClipboardError(e.to_string()),
                    };
                    let effects = self.sm.handle(event);
                    self.execute_effects(effects, ctx);
                }
                TapAction::Pending => {}
            }
        }
    }

    // -- Diagnostics scenario action handling (from runner thread) --

    #[cfg(feature = "diagnostics")]
    fn poll_diag_actions(&mut self, ctx: &egui::Context) {
        while let Ok(action) = self.diag_action_rx.try_recv() {
            match action {
                crate::diagnostics::ScenarioAction::ShowOverlay { mode, text } => {
                    // Switch mode first (no-op effects in Hidden state) before ContentReady.
                    self.sm.handle(UiEvent::UserSwitchMode(mode));
                    let effects = self.sm.handle(UiEvent::ContentReady {
                        content: crate::ClipboardContent::text_only(text),
                        auto_copy: true,
                    });
                    self.execute_effects(effects, ctx);
                }
                crate::diagnostics::ScenarioAction::SwitchMode(mode) => {
                    let effects = self.sm.handle(UiEvent::UserSwitchMode(mode));
                    self.execute_effects(effects, ctx);
                }
                crate::diagnostics::ScenarioAction::HideOverlay => {
                    let effects = self.sm.handle(UiEvent::UserClose);
                    self.execute_effects(effects, ctx);
                }
                crate::diagnostics::ScenarioAction::Quit => {
                    info!("diag: all scenarios finished, exiting");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                crate::diagnostics::ScenarioAction::None => {}
            }
        }
    }

    // -- Worker response polling --

    fn poll_responses(&mut self, ctx: &egui::Context) {
        while let Ok(response) = self.resp_rx.try_recv() {
            let event = match response {
                WorkerResponse::Complete { result, think_content, request_id } => {
                    UiEvent::WorkerResult {
                        text: result,
                        think_content,
                        request_id,
                    }
                }
                WorkerResponse::Error { message, request_id } => {
                    UiEvent::WorkerError {
                        message,
                        request_id,
                    }
                }
                WorkerResponse::StreamDelta { text, request_id } => {
                    UiEvent::StreamDelta { text, request_id }
                }
                WorkerResponse::ThinkStarted { request_id } => {
                    UiEvent::ThinkStarted { request_id }
                }
                WorkerResponse::ThinkingProbeResult { supported } => {
                    UiEvent::ThinkingProbeResult(supported)
                }
            };
            let effects = self.sm.handle(event);
            self.execute_effects(effects, ctx);
        }
    }

    // -- Focus handling --

    fn check_focus_lost(&mut self, ctx: &egui::Context) {
        if matches!(self.sm.state(), OverlayState::Hidden) {
            return;
        }
        let focused = ctx.input(|i| i.viewport().focused);
        if focused == Some(true) {
            self.sm.handle(UiEvent::FocusGained);
        } else if focused == Some(false) {
            let effects = self.sm.handle(UiEvent::FocusLost);
            self.execute_effects(effects, ctx);
        }
    }

    // -- Window management (platform / egui dependent) --

    fn capture_mouse_position(&mut self) {
        // Skip if already set for this show cycle (e.g., from coordinator
        // first-press capture via TapEvent). Cleared on HideWindow.
        if self.spawn_position.is_some() {
            return;
        }
        self.spawn_position = self
            .platform
            .mouse_position()
            .map(|(x, y)| egui::pos2(x as f32, y as f32));
    }

    /// Calculate centered-and-clamped window position for `spawn_position`.
    /// Returns top-left corner in screen coordinates (Quartz on macOS, logical on Windows).
    fn calculate_centered_position(&self, win_size: egui::Vec2) -> Option<egui::Pos2> {
        let cursor = self.spawn_position?;
        let bounds = self.platform.display_bounds_at_point(cursor.x as f64, cursor.y as f64);
        Some(center_clamped_to_bounds(cursor, win_size, bounds))
    }

    /// Reposition the window while the overlay is already visible (e.g. after size change).
    ///
    /// Delegates to the platform for native DPI-safe repositioning (e.g. Windows SetWindowPos
    /// bypasses winit's per-monitor scaling). Falls back to ViewportCommand::OuterPosition.
    fn reposition_window(&self, ctx: &egui::Context, win_size: egui::Vec2) {
        if let Some(pos) = self.calculate_centered_position(win_size) {
            if !self.platform.reposition_window(pos.x, pos.y) {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            }
        }
    }

    fn show_window(&self, ctx: &egui::Context) {
        // Skip repositioning if user has manually dragged the window;
        // only reposition on initial show (before any drag).
        let pos = if self.sm.user_repositioned() {
            None
        } else {
            self.last_desired_size
                .and_then(|s| self.calculate_centered_position(s))
                .map(|p| (p.x, p.y))
        };

        if self.platform.show_window(pos) {
            // Windows: sync winit to visible=true to maintain ControlFlow::Wait (egui#5229).
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
    }

    fn hide_window(&self, ctx: &egui::Context) {
        if !self.platform.hide_window() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
    }

    // -- update() helpers --

    /// Hide the window on the very first frame so the overlay is not visible at startup.
    fn maybe_initial_hide(&mut self, ctx: &egui::Context) {
        if !self.initial_hide_done {
            self.initial_hide_done = true;
            self.hide_window(ctx);
        }
    }

    /// Resize the viewport when the desired content size changes, then reposition.
    fn update_viewport(&mut self, ctx: &egui::Context, desired: Option<egui::Vec2>) {
        let Some(size) = desired else { return };
        if self.last_desired_size != Some(size) {
            self.last_desired_size = Some(size);
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }
        if !matches!(self.sm.state(), OverlayState::Hidden) && !self.sm.user_repositioned() {
            self.reposition_window(ctx, size);
        }
    }

    /// Translate the overlay action returned by `render()` into state machine events.
    fn handle_overlay_action(&mut self, ctx: &egui::Context, action: overlay::OverlayAction) {
        let event = match action {
            overlay::OverlayAction::None => return,
            overlay::OverlayAction::Close => UiEvent::UserClose,
            overlay::OverlayAction::Cancel => UiEvent::UserCancel,
            overlay::OverlayAction::StartDrag => {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                UiEvent::UserStartDrag
            }
            overlay::OverlayAction::SwitchMode(mode) => UiEvent::UserSwitchMode(mode),
            overlay::OverlayAction::ToggleThink => {
                self.think_expanded = !self.think_expanded;
                return;
            }
            overlay::OverlayAction::ChangeRephraseStyle(style) => {
                UiEvent::UserChangeRephraseStyle(style)
            }
            overlay::OverlayAction::ChangeRephraseLength(length) => {
                UiEvent::UserChangeRephraseLength(length)
            }
            overlay::OverlayAction::ChangeThinkingMode(thinking) => {
                UiEvent::UserChangeThinkingMode(thinking)
            }
            overlay::OverlayAction::CopyToClipboard => UiEvent::UserCopy,
        };
        let effects = self.sm.handle(event);
        self.execute_effects(effects, ctx);
    }

    /// Request repaints: every frame while Processing (spinner), idle poll in diagnostics mode.
    fn schedule_repaint(&self, ctx: &egui::Context) {
        if matches!(self.sm.state(), OverlayState::Processing) {
            ctx.request_repaint();
        } else {
            #[cfg(feature = "diagnostics")]
            ctx.request_repaint_after(std::time::Duration::from_millis(IDLE_POLL_MS));
        }
    }
}

impl eframe::App for OverlayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.maybe_initial_hide(ctx);

        self.poll_responses(ctx);
        self.poll_tap_actions(ctx);
        #[cfg(feature = "diagnostics")]
        self.poll_diag_actions(ctx);

        // Diagnostics: receive screenshot events.
        #[cfg(feature = "diagnostics")]
        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::Screenshot { image, .. } = event {
                    self.diag.on_screenshot(image);
                }
            }
        });

        let output = overlay::render(
            self.sm.state(),
            self.sm.mode(),
            overlay::StreamingState {
                text: self.sm.streaming_text(),
                think_started: self.sm.think_started(),
                think_content: self.sm.think_content(),
                think_expanded: self.think_expanded,
            },
            self.sm.available_modes(),
            self.sm.rephrase_params(),
            overlay::ThinkingState {
                mode: self.sm.effective_thinking_mode(),
                supported: self.sm.thinking_supported(),
            },
            ctx,
        );

        self.update_viewport(ctx, output.desired_size);
        self.handle_overlay_action(ctx, output.action);

        // Diagnostics: record frame data + flush stale screenshots.
        #[cfg(feature = "diagnostics")]
        {
            use crate::diagnostics::FrameSnapshot;
            self.diag.record_frame(FrameSnapshot {
                frame: self.diag.frame_counter(),
                state: self.sm.variant_name(),
                mode: self.sm.mode().label(),
                content_size: output.content_size.map(|v| [v.x, v.y]),
                desired_size: output.desired_size.map(|v| [v.x, v.y]),
                viewport_inner_rect: ctx
                    .input(|i| i.viewport().inner_rect)
                    .map(|r| [r.min.x, r.min.y, r.max.x, r.max.y]),
                spawn_position: self.spawn_position.map(|p| [p.x, p.y]),
                user_repositioned: self.sm.user_repositioned(),
            });
            self.diag.tick_screenshot(ctx);
            self.diag.flush_pending_if_stale();
        }

        crate::platform::poll_tray_quit(ctx);

        // Focus-loss auto-hide (skip during diagnostics).
        #[cfg(not(feature = "diagnostics"))]
        self.check_focus_lost(ctx);

        self.schedule_repaint(ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}

/// Center `win_size` on `cursor` and clamp the result within `bounds`.
///
/// `bounds` is `(origin_x, origin_y, width, height)` in the same coordinate space
/// as `cursor`. Returns the top-left corner of the positioned window.
///
/// Extracted as a free function so the clamping logic can be unit-tested without
/// a live platform or egui context.
fn center_clamped_to_bounds(
    cursor: egui::Pos2,
    win_size: egui::Vec2,
    bounds: Option<(f64, f64, f64, f64)>,
) -> egui::Pos2 {
    let mut x = cursor.x - win_size.x / 2.0;
    let mut y = cursor.y - win_size.y / 2.0;

    if let Some((ox, oy, w, h)) = bounds {
        let (ox, oy, w, h) = (ox as f32, oy as f32, w as f32, h as f32);
        x = x.clamp(ox, (ox + w - win_size.x).max(ox));
        y = y.clamp(oy, (oy + h - win_size.y).max(oy));
    }

    egui::pos2(x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(ox: f64, oy: f64, w: f64, h: f64) -> Option<(f64, f64, f64, f64)> {
        Some((ox, oy, w, h))
    }

    // --- no bounds: pure cursor centering ---

    #[test]
    fn no_bounds_centers_on_cursor() {
        let pos = center_clamped_to_bounds(egui::pos2(1000.0, 500.0), egui::vec2(400.0, 300.0), None);
        assert_eq!(pos, egui::pos2(800.0, 350.0));
    }

    // --- primary monitor (origin at 0,0) ---

    #[test]
    fn primary_monitor_cursor_centered() {
        // cursor well inside 2560×1440, overlay 600×400 → no clamping
        let pos = center_clamped_to_bounds(
            egui::pos2(1280.0, 720.0),
            egui::vec2(600.0, 400.0),
            bounds(0.0, 0.0, 2560.0, 1440.0),
        );
        assert_eq!(pos, egui::pos2(980.0, 520.0));
    }

    #[test]
    fn primary_monitor_clamp_right_edge() {
        // cursor near right edge → clamp so window stays on-screen
        let pos = center_clamped_to_bounds(
            egui::pos2(2500.0, 720.0),
            egui::vec2(600.0, 400.0),
            bounds(0.0, 0.0, 2560.0, 1440.0),
        );
        // max_x = 0 + 2560 - 600 = 1960
        assert_eq!(pos.x, 1960.0);
        assert_eq!(pos.y, 520.0);
    }

    #[test]
    fn primary_monitor_clamp_bottom_edge() {
        let pos = center_clamped_to_bounds(
            egui::pos2(1280.0, 1400.0),
            egui::vec2(600.0, 400.0),
            bounds(0.0, 0.0, 2560.0, 1440.0),
        );
        // max_y = 0 + 1440 - 400 = 1040
        assert_eq!(pos.y, 1040.0);
    }

    #[test]
    fn primary_monitor_clamp_top_left_corner() {
        let pos = center_clamped_to_bounds(
            egui::pos2(0.0, 0.0),
            egui::vec2(600.0, 400.0),
            bounds(0.0, 0.0, 2560.0, 1440.0),
        );
        // raw = (-300, -200) → clamped to (0, 0)
        assert_eq!(pos, egui::pos2(0.0, 0.0));
    }

    // --- secondary monitor (offset origin, different DPI scale) ---

    #[test]
    fn secondary_monitor_cursor_centered() {
        // Secondary monitor placed to the right: logical origin (2560, 0), size 1920×1080.
        // Simulates the multi-monitor DPI bug scenario: cursor at logical (3500, 500).
        let pos = center_clamped_to_bounds(
            egui::pos2(3500.0, 500.0),
            egui::vec2(600.0, 400.0),
            bounds(2560.0, 0.0, 1920.0, 1080.0),
        );
        // centered: (3200, 300); within bounds [2560..3880, 0..680] → no clamp
        assert_eq!(pos, egui::pos2(3200.0, 300.0));
    }

    #[test]
    fn secondary_monitor_clamp_right_edge() {
        // cursor near right edge of secondary monitor
        let pos = center_clamped_to_bounds(
            egui::pos2(4400.0, 500.0),
            egui::vec2(600.0, 400.0),
            bounds(2560.0, 0.0, 1920.0, 1080.0),
        );
        // max_x = 2560 + 1920 - 600 = 3880
        assert_eq!(pos.x, 3880.0);
    }

    #[test]
    fn secondary_monitor_clamp_left_edge() {
        // cursor at left edge of secondary monitor
        let pos = center_clamped_to_bounds(
            egui::pos2(2560.0, 500.0),
            egui::vec2(600.0, 400.0),
            bounds(2560.0, 0.0, 1920.0, 1080.0),
        );
        // raw x = 2560 - 300 = 2260 < 2560 → clamp to 2560
        assert_eq!(pos.x, 2560.0);
    }

    // --- window larger than monitor (degenerate guard) ---

    #[test]
    fn window_wider_than_monitor_clamps_to_origin() {
        // win_size.x > monitor width → max(ox) guard prevents negative clamp bound
        let pos = center_clamped_to_bounds(
            egui::pos2(100.0, 100.0),
            egui::vec2(2000.0, 400.0),
            bounds(0.0, 0.0, 800.0, 600.0),
        );
        // max_x = max(0, 0 + 800 - 2000) = max(0, -1200) = 0
        assert_eq!(pos.x, 0.0);
    }

    // --- negative-origin monitors (macOS vertical stacks, Windows left/above primary) ---

    #[test]
    fn monitor_left_of_primary_negative_x_origin() {
        // Secondary monitor to the left of primary: origin at (-1920, 0).
        let pos = center_clamped_to_bounds(
            egui::pos2(-960.0, 540.0),
            egui::vec2(600.0, 400.0),
            bounds(-1920.0, 0.0, 1920.0, 1080.0),
        );
        // centered: x = -960 - 300 = -1260, clamp to [-1920, -1920+1920-600] = [-1920, -600]
        // -1260 is within [-1920, -600] → no clamp
        assert_eq!(pos.x, -1260.0);
        // centered: y = 540 - 200 = 340, clamp to [0, 680] → 340
        assert_eq!(pos.y, 340.0);
    }

    #[test]
    fn monitor_above_primary_negative_y_origin() {
        // Secondary monitor above primary: origin at (0, -1080).
        let pos = center_clamped_to_bounds(
            egui::pos2(1280.0, -540.0),
            egui::vec2(600.0, 400.0),
            bounds(0.0, -1080.0, 2560.0, 1080.0),
        );
        // centered: y = -540 - 200 = -740, clamp to [-1080, -1080+1080-400] = [-1080, -400]
        // -740 is within [-1080, -400] → no clamp
        assert_eq!(pos.y, -740.0);
    }

    // Without display bounds, centering on a cursor near the screen origin produces
    // negative top-left coordinates. This is intentional: the OS will render the
    // window partially off-screen, which is acceptable without monitor clamping info.
    #[test]
    fn no_bounds_result_is_negative_near_origin() {
        let pos = center_clamped_to_bounds(
            egui::pos2(10.0, 10.0),
            egui::vec2(600.0, 400.0),
            None,
        );
        assert_eq!(pos, egui::pos2(-290.0, -190.0));
    }
}
