mod overlay;

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
use crate::ProcessMode;

/// Polling interval when overlay is hidden (for hotkey detection).
const IDLE_POLL_MS: u64 = 100;

#[derive(Debug)]
pub enum OverlayState {
    Hidden,
    Processing,
    Result(String),
    Error(String),
}

pub struct OverlayApp {
    state: OverlayState,
    mode: ProcessMode,
    /// Original input text, retained for re-processing on mode switch.
    original_text: Option<String>,
    cmd_tx: tokio_mpsc::UnboundedSender<WorkerCommand>,
    resp_rx: mpsc::Receiver<WorkerResponse>,
    clipboard: ClipboardManager,
    platform: NativePlatform,
    detector: HotkeyDetector,
    /// True once the window has received focus after show_window.
    /// Only check for focus loss after this becomes true.
    has_been_focused: bool,
    /// Mouse cursor position captured at hotkey trigger time (egui logical points).
    spawn_position: Option<egui::Pos2>,
    /// True after the user drags the overlay; suppresses automatic repositioning.
    user_repositioned: bool,
    /// Tracks state variant changes to reset egui Area sizing on transitions.
    prev_state_disc: std::mem::Discriminant<OverlayState>,
}

impl OverlayApp {
    pub fn new(
        cmd_tx: tokio_mpsc::UnboundedSender<WorkerCommand>,
        resp_rx: mpsc::Receiver<WorkerResponse>,
        clipboard: ClipboardManager,
    ) -> Self {
        Self {
            state: OverlayState::Hidden,
            mode: ProcessMode::default(),
            original_text: None,
            cmd_tx,
            resp_rx,
            clipboard,
            platform: NativePlatform,
            detector: HotkeyDetector::new(),
            has_been_focused: false,
            spawn_position: None,
            user_repositioned: false,
            prev_state_disc: std::mem::discriminant(&OverlayState::Hidden),
        }
    }

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
        match self.clipboard.read_clipboard() {
            Ok(text) => self.start_processing(text, ctx),
            Err(e) => self.show_error(e.to_string(), ctx),
        }
    }

    fn trigger_double_tap(&mut self, ctx: &egui::Context) {
        match self.clipboard.copy_and_read(&self.platform) {
            Ok(text) => self.start_processing(text, ctx),
            Err(e) => self.show_error(e.to_string(), ctx),
        }
    }

    fn capture_mouse_position(&mut self) {
        self.spawn_position = self
            .platform
            .mouse_position()
            .map(|(x, y)| egui::pos2(x as f32, y as f32));
    }

    fn start_processing(&mut self, text: String, ctx: &egui::Context) {
        info!("starting {} ({} chars)", self.mode.label(), text.len());
        self.original_text = Some(text.clone());
        let _ = self.cmd_tx.send(WorkerCommand::Process {
            text,
            mode: self.mode,
        });
        self.state = OverlayState::Processing;
        self.capture_mouse_position();
        self.user_repositioned = false;
        self.show_window(ctx);
    }

    fn switch_mode(&mut self, new_mode: ProcessMode) {
        if self.mode == new_mode {
            return;
        }
        self.mode = new_mode;

        match &self.state {
            OverlayState::Processing => {
                // Cancel current request and re-send with new mode.
                let _ = self.cmd_tx.send(WorkerCommand::Cancel);
                if let Some(text) = self.original_text.clone() {
                    let _ = self.cmd_tx.send(WorkerCommand::Process {
                        text,
                        mode: self.mode,
                    });
                }
            }
            OverlayState::Result(_) | OverlayState::Error(_) => {
                // Re-process original text with new mode.
                if let Some(text) = self.original_text.clone() {
                    let _ = self.cmd_tx.send(WorkerCommand::Process {
                        text,
                        mode: self.mode,
                    });
                    self.state = OverlayState::Processing;
                }
            }
            OverlayState::Hidden => {}
        }
    }

    fn show_error(&mut self, message: String, ctx: &egui::Context) {
        error!("pipeline error: {message}");
        self.state = OverlayState::Error(message);
        self.capture_mouse_position();
        self.user_repositioned = false;
        self.show_window(ctx);
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

    fn show_window(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "macos")]
        crate::platform::macos::configure_window_for_spaces();

        if !self.user_repositioned {
            let win_size = ctx
                .input(|i| i.viewport().inner_rect)
                .map(|r| r.size())
                .unwrap_or(egui::vec2(400.0, 120.0));
            self.reposition_window(ctx, win_size);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.has_been_focused = false;
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.state = OverlayState::Hidden;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }

    fn poll_responses(&mut self, ctx: &egui::Context) {
        while let Ok(response) = self.resp_rx.try_recv() {
            match response {
                WorkerResponse::Complete { result } => {
                    if let Err(e) = self.clipboard.write_text(&result) {
                        self.state = OverlayState::Error(e.to_string());
                    } else {
                        info!("{} complete ({} chars), copied to clipboard", self.mode.label(), result.len());
                        self.state = OverlayState::Result(result);
                    }
                }
                WorkerResponse::Error { message } => {
                    error!("worker error: {message}");
                    self.state = OverlayState::Error(message);
                }
            }
            // Ensure window is visible for result/error.
            self.show_window(ctx);
        }
    }

    fn check_focus_lost(&mut self, ctx: &egui::Context) {
        if matches!(self.state, OverlayState::Hidden) {
            return;
        }
        let focused = ctx.input(|i| i.viewport().focused);
        if focused == Some(true) {
            self.has_been_focused = true;
        } else if focused == Some(false) && self.has_been_focused {
            // Only close after we've confirmed the window had focus at least once.
            self.hide_window(ctx);
        }
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
        if matches!(self.state, OverlayState::Hidden) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }

        self.poll_responses(ctx);
        self.poll_hotkeys(ctx);

        // Reset egui Area stored sizing when state variant changes.
        // egui::Area persists the previous frame's content size as the next frame's
        // max_rect. Without this reset, transitioning from a short state (Processing)
        // to a tall one (Result) would starve the ScrollArea of vertical space.
        let disc = std::mem::discriminant(&self.state);
        if disc != self.prev_state_disc {
            ctx.memory_mut(|m| m.reset_areas());
            self.prev_state_disc = disc;
        }

        let output = overlay::render(&self.state, self.mode, ctx);

        // Resize viewport to fit rendered content.
        if let Some(desired) = output.desired_size {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));

            if !matches!(self.state, OverlayState::Hidden) && !self.user_repositioned {
                self.reposition_window(ctx, desired);
            }
        }

        match output.action {
            overlay::OverlayAction::None => {}
            overlay::OverlayAction::Close => {
                self.hide_window(ctx);
            }
            overlay::OverlayAction::Cancel => {
                let _ = self.cmd_tx.send(WorkerCommand::Cancel);
                self.hide_window(ctx);
            }
            overlay::OverlayAction::StartDrag => {
                self.user_repositioned = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            overlay::OverlayAction::SwitchMode(new_mode) => {
                self.switch_mode(new_mode);
            }
        }

        self.check_focus_lost(ctx);

        // Schedule next repaint.
        match &self.state {
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
