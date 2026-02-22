mod overlay;
pub mod state_machine;

use std::sync::mpsc;
use std::time::Duration;

use tokio::sync::mpsc as tokio_mpsc;

use eframe::egui;
use tracing::{debug, error, info};

use crate::clipboard::ClipboardManager;
use crate::hotkey::TapAction;
use crate::platform::{NativePlatform, Platform};
use crate::worker::{WorkerCommand, WorkerResponse};

pub use state_machine::OverlayState;
use state_machine::{StateMachine, UiEffect, UiEvent};

/// Polling interval for diagnostics scenario runner.
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
    /// Tap actions from coordinator thread (hotkey detection runs off-UI).
    tap_rx: mpsc::Receiver<TapAction>,
    /// Cached desired_size to avoid redundant send_viewport_cmd calls.
    last_desired_size: Option<egui::Vec2>,
    #[cfg(feature = "diagnostics")]
    diag: crate::diagnostics::DiagCollector,
    #[cfg(feature = "diagnostics")]
    scenario_runner: crate::diagnostics::DiagScenarioRunner,
    #[cfg(feature = "diagnostics")]
    prev_state_name: &'static str,
}

impl OverlayApp {
    pub fn new(
        cmd_tx: tokio_mpsc::UnboundedSender<WorkerCommand>,
        resp_rx: mpsc::Receiver<WorkerResponse>,
        clipboard: ClipboardManager,
        tap_rx: mpsc::Receiver<TapAction>,
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
            #[cfg(feature = "diagnostics")]
            diag: crate::diagnostics::DiagCollector::new(),
            #[cfg(feature = "diagnostics")]
            scenario_runner: crate::diagnostics::DiagScenarioRunner::new(),
            #[cfg(feature = "diagnostics")]
            prev_state_name: "Hidden",
        }
    }

    // -- Effect execution --

    fn execute_effects(&mut self, effects: Vec<UiEffect>, ctx: &egui::Context) {
        for effect in effects {
            match effect {
                UiEffect::SendProcess {
                    content,
                    mode,
                    request_id,
                } => {
                    let text_len = content.text.as_ref().map_or(0, |t| t.len());
                    let img_count = content.images.len();
                    info!("starting {} ({} chars, {} images)", mode.label(), text_len, img_count);
                    let _ = self.cmd_tx.send(WorkerCommand::Process {
                        content,
                        mode,
                        request_id,
                    });
                }
                UiEffect::SendCancel => {
                    let _ = self.cmd_tx.send(WorkerCommand::Cancel);
                }
                UiEffect::WriteClipboard(text) => {
                    if let Err(e) = self.clipboard.write_text(&text) {
                        error!("clipboard write failed: {e}");
                        let err_effects =
                            self.sm.handle(UiEvent::ClipboardWriteError(e.to_string()));
                        // ClipboardWriteError never emits WriteClipboard — recursion safe.
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
                UiEffect::HideWindow => self.hide_window(ctx),
                UiEffect::CaptureMousePosition => self.capture_mouse_position(),
                UiEffect::ResetAreas => {
                    #[cfg(feature = "diagnostics")]
                    {
                        let to = self.sm.variant_name();
                        self.diag
                            .on_state_transition(self.prev_state_name, to);
                        self.prev_state_name = to;
                    }
                    ctx.memory_mut(|m| m.reset_areas());
                }
            }
        }
    }

    // -- Tap action handling (from coordinator thread) --

    fn poll_tap_actions(&mut self, ctx: &egui::Context) {
        while let Ok(action) = self.tap_rx.try_recv() {
            match action {
                TapAction::SingleTap => {
                    info!("single-tap triggered, using clipboard content...");
                    let event = match self.clipboard.read_content() {
                        Ok(content) => UiEvent::ContentReady(content),
                        Err(e) => UiEvent::ClipboardWriteError(e.to_string()),
                    };
                    let effects = self.sm.handle(event);
                    self.execute_effects(effects, ctx);
                }
                TapAction::DoubleTap => {
                    info!("double-tap triggered, copying selection...");
                    let event = match self.clipboard.copy_and_read(&self.platform) {
                        Ok(content) => UiEvent::ContentReady(content),
                        Err(e) => UiEvent::ClipboardWriteError(e.to_string()),
                    };
                    let effects = self.sm.handle(event);
                    self.execute_effects(effects, ctx);
                }
                TapAction::Pending => {}
            }
        }
    }

    // -- Worker response polling --

    fn poll_responses(&mut self, ctx: &egui::Context) {
        while let Ok(response) = self.resp_rx.try_recv() {
            let event = match response {
                WorkerResponse::Complete { result, request_id } => {
                    UiEvent::WorkerResult {
                        text: result,
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
            self.sm.set_focused();
        } else if focused == Some(false) {
            let effects = self.sm.handle(UiEvent::FocusLost);
            self.execute_effects(effects, ctx);
        }
    }

    // -- Window management (platform / egui dependent) --

    fn capture_mouse_position(&mut self) {
        self.spawn_position = self
            .platform
            .mouse_position()
            .map(|(x, y)| egui::pos2(x as f32, y as f32));
    }

    /// Reposition the window so it is centered on `spawn_position`,
    /// clamped to the display containing the cursor.
    fn reposition_window(&self, ctx: &egui::Context, win_size: egui::Vec2) {
        if let Some(cursor) = self.spawn_position {
            let mut x = cursor.x - win_size.x / 2.0;
            let mut y = cursor.y - win_size.y / 2.0;

            #[cfg(target_os = "macos")]
            if let Some((ox, oy, w, h)) = crate::platform::macos::display_bounds_at_point(
                cursor.x as f64,
                cursor.y as f64,
            ) {
                let (ox, oy, w, h) = (ox as f32, oy as f32, w as f32, h as f32);
                x = x.clamp(ox, (ox + w - win_size.x).max(ox));
                y = y.clamp(oy, (oy + h - win_size.y).max(oy));
            }

            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
        }
    }

    #[allow(unused_variables)]
    fn show_window(&self, ctx: &egui::Context) {
        #[cfg(target_os = "macos")]
        {
            crate::platform::macos::configure_window_for_spaces();
            crate::platform::macos::show_and_focus_window();
        }

        #[cfg(target_os = "windows")]
        crate::platform::windows::show_and_focus_window();

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    #[allow(unused_variables)]
    fn hide_window(&self, ctx: &egui::Context) {
        // Diagnostics: keep window visible so update() keeps firing on Windows
        // (WM_PAINT not delivered to hidden windows).
        #[cfg(not(feature = "diagnostics"))]
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }
}

impl eframe::App for OverlayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Ensure window is hidden at startup.
        // Fixes macOS startup where with_visible(false) doesn't fully suppress the window.
        // Diagnostics: skip hide so update() keeps firing on Windows (WM_PAINT issue).
        #[cfg(not(feature = "diagnostics"))]
        if !self.initial_hide_done && matches!(self.sm.state(), OverlayState::Hidden) {
            debug!("update() first call — hiding window");
            self.hide_window(ctx);
            self.initial_hide_done = true;
        }
        #[cfg(feature = "diagnostics")]
        {
            self.initial_hide_done = true;
        }

        // Process worker responses.
        self.poll_responses(ctx);

        // Process tap actions from coordinator thread.
        self.poll_tap_actions(ctx);

        // Diagnostics: drive scenario runner via state machine.
        #[cfg(feature = "diagnostics")]
        {
            let state_name = self.sm.variant_name();
            match self.scenario_runner.tick(state_name) {
                crate::diagnostics::ScenarioAction::None => {}
                crate::diagnostics::ScenarioAction::ShowOverlay { mode, text } => {
                    self.sm.set_mode(mode);
                    let effects = self.sm.handle(UiEvent::ContentReady(
                        crate::ClipboardContent::text_only(text),
                    ));
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
            }
        }

        // Diagnostics: receive screenshot events.
        #[cfg(feature = "diagnostics")]
        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::Screenshot { image, .. } = event {
                    self.diag.on_screenshot(image);
                }
            }
        });

        // Render overlay.
        let output = overlay::render(
            self.sm.state(),
            self.sm.mode(),
            self.sm.streaming_text(),
            self.sm.available_modes(),
            ctx,
        );

        // Resize viewport to fit rendered content (only when size changes).
        if let Some(desired) = output.desired_size {
            let size_changed = self.last_desired_size != Some(desired);
            if size_changed {
                self.last_desired_size = Some(desired);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));
            }

            if size_changed
                && !matches!(self.sm.state(), OverlayState::Hidden)
                && !self.sm.user_repositioned()
            {
                self.reposition_window(ctx, desired);
            }
        } else {
            self.last_desired_size = None;
        }

        // Handle overlay UI actions.
        let event = match output.action {
            overlay::OverlayAction::Close => Some(UiEvent::UserClose),
            overlay::OverlayAction::Cancel => Some(UiEvent::UserCancel),
            overlay::OverlayAction::SwitchMode(m) => Some(UiEvent::UserSwitchMode(m)),
            overlay::OverlayAction::StartDrag => {
                self.sm.set_user_repositioned();
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                None
            }
            overlay::OverlayAction::None => None,
        };
        if let Some(ev) = event {
            let effects = self.sm.handle(ev);
            self.execute_effects(effects, ctx);
        }

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

        // Focus-loss auto-hide (skip during diagnostics).
        #[cfg(feature = "diagnostics")]
        let skip_focus_check = true;
        #[cfg(not(feature = "diagnostics"))]
        let skip_focus_check = false;

        if !skip_focus_check {
            self.check_focus_lost(ctx);
        }

        // Schedule next repaint.
        #[cfg(feature = "diagnostics")]
        let force_poll = true; // diagnostics needs periodic tick() calls
        #[cfg(not(feature = "diagnostics"))]
        let force_poll = false;

        match self.sm.state() {
            OverlayState::Processing => {
                ctx.request_repaint(); // spinner animation + streaming updates
            }
            _ => {
                // Hotkey polling is handled by coordinator thread — UI is event-driven only.
                // Diagnostics needs periodic tick() calls for scenario runner.
                if force_poll {
                    ctx.request_repaint_after(Duration::from_millis(IDLE_POLL_MS));
                }
            }
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}
