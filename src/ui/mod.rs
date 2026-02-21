mod overlay;
pub mod state_machine;

use std::sync::mpsc;
use std::time::Duration;

use tokio::sync::mpsc as tokio_mpsc;

use eframe::egui;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use tracing::{error, info};

use crate::clipboard::ClipboardManager;
use crate::hotkey::{HotkeyDetector, TapAction};
use crate::platform::{NativePlatform, Platform};
use crate::worker::{WorkerCommand, WorkerResponse};

pub use state_machine::OverlayState;
use state_machine::{StateMachine, UiEffect, UiEvent};

/// Polling interval when overlay is hidden (for hotkey detection).
const IDLE_POLL_MS: u64 = 100;

pub struct OverlayApp {
    sm: StateMachine,
    cmd_tx: tokio_mpsc::UnboundedSender<WorkerCommand>,
    resp_rx: mpsc::Receiver<WorkerResponse>,
    clipboard: ClipboardManager,
    platform: NativePlatform,
    detector: HotkeyDetector,
    /// Mouse cursor position captured at hotkey trigger time.
    spawn_position: Option<egui::Pos2>,
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
    ) -> Self {
        Self {
            sm: StateMachine::new(crate::ProcessMode::default()),
            cmd_tx,
            resp_rx,
            clipboard,
            platform: NativePlatform,
            detector: HotkeyDetector::new(),
            spawn_position: None,
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
                    text,
                    mode,
                    request_id,
                } => {
                    info!("starting {} ({} chars)", mode.label(), text.len());
                    let _ = self.cmd_tx.send(WorkerCommand::Process {
                        text,
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

    // -- Hotkey handling --

    fn poll_hotkeys(&mut self, ctx: &egui::Context) {
        let receiver = GlobalHotKeyEvent::receiver();
        while let Ok(event) = receiver.try_recv() {
            if event.state != HotKeyState::Pressed {
                continue;
            }
            match self.detector.on_press() {
                TapAction::Pending => {}
                TapAction::DoubleTap => {
                    info!("double-tap triggered, copying selection...");
                    self.trigger_double_tap(ctx);
                }
            }
        }
        if self.detector.check_timeout() {
            info!("single-tap triggered, using clipboard content...");
            self.trigger_single_tap(ctx);
        }
    }

    fn trigger_single_tap(&mut self, ctx: &egui::Context) {
        let event = match self.clipboard.read_clipboard() {
            Ok(text) => UiEvent::TextReady(text),
            Err(e) => UiEvent::ClipboardWriteError(e.to_string()),
        };
        let effects = self.sm.handle(event);
        self.execute_effects(effects, ctx);
    }

    fn trigger_double_tap(&mut self, ctx: &egui::Context) {
        let event = match self.clipboard.copy_and_read(&self.platform) {
            Ok(text) => UiEvent::TextReady(text),
            Err(e) => UiEvent::ClipboardWriteError(e.to_string()),
        };
        let effects = self.sm.handle(event);
        self.execute_effects(effects, ctx);
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

    fn show_window(&self, ctx: &egui::Context) {
        #[cfg(target_os = "macos")]
        crate::platform::macos::configure_window_for_spaces();

        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn hide_window(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }
}

impl eframe::App for OverlayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Transparent background for the entire viewport.
        ctx.set_visuals(egui::Visuals {
            window_fill: egui::Color32::TRANSPARENT,
            panel_fill: egui::Color32::TRANSPARENT,
            window_stroke: egui::Stroke::NONE,
            window_shadow: egui::Shadow::NONE,
            window_corner_radius: egui::CornerRadius::same(12),
            ..egui::Visuals::dark()
        });

        // Ensure window is hidden when state is Hidden.
        // Fixes macOS startup where with_visible(false) doesn't fully suppress the window.
        if matches!(self.sm.state(), OverlayState::Hidden) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        // 1. Process worker responses.
        self.poll_responses(ctx);

        // 2. Process hotkeys.
        self.poll_hotkeys(ctx);

        // 3. Diagnostics: drive scenario runner.
        #[cfg(feature = "diagnostics")]
        {
            let state_name = self.sm.variant_name();
            match self.scenario_runner.tick(state_name, &self.cmd_tx) {
                crate::diagnostics::ScenarioAction::None => {}
                crate::diagnostics::ScenarioAction::ShowOverlay { mode } => {
                    self.sm.set_mode(mode);
                    let effects = self.sm.handle(UiEvent::TextReady(
                        // Scenario runner already sent the Process command;
                        // we need the text for state machine bookkeeping.
                        // TODO(step4): refactor runner to not send commands directly.
                        String::new(),
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

        // 4. Diagnostics: receive screenshot events.
        #[cfg(feature = "diagnostics")]
        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::Screenshot { image, .. } = event {
                    self.diag.on_screenshot(image);
                }
            }
        });

        // 5. Render overlay.
        let output = overlay::render(self.sm.state(), self.sm.mode(), ctx);

        // 6. Resize viewport to fit rendered content.
        if let Some(desired) = output.desired_size {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));

            if !matches!(self.sm.state(), OverlayState::Hidden) && !self.sm.user_repositioned() {
                self.reposition_window(ctx, desired);
            }
        }

        // 7. Handle overlay UI actions.
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

        // 8. Diagnostics: record frame data + flush stale screenshots.
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

        // 9. Focus-loss auto-hide (skip during diagnostics).
        #[cfg(feature = "diagnostics")]
        let skip_focus_check = true;
        #[cfg(not(feature = "diagnostics"))]
        let skip_focus_check = false;

        if !skip_focus_check {
            self.check_focus_lost(ctx);
        }

        // 10. Schedule next repaint.
        match self.sm.state() {
            OverlayState::Hidden => {
                ctx.request_repaint_after(Duration::from_millis(IDLE_POLL_MS));
            }
            _ => {
                ctx.request_repaint();
            }
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}
