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
}

impl OverlayApp {
    pub fn new(
        cmd_tx: tokio_mpsc::UnboundedSender<WorkerCommand>,
        resp_rx: mpsc::Receiver<WorkerResponse>,
        clipboard: ClipboardManager,
    ) -> Self {
        Self {
            state: OverlayState::Hidden,
            cmd_tx,
            resp_rx,
            clipboard,
            platform: NativePlatform,
            detector: HotkeyDetector::new(),
            has_been_focused: false,
            spawn_position: None,
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
            Ok(text) => self.start_translation(text, ctx),
            Err(e) => self.show_error(e.to_string(), ctx),
        }
    }

    fn trigger_double_tap(&mut self, ctx: &egui::Context) {
        match self.clipboard.copy_and_read(&self.platform) {
            Ok(text) => self.start_translation(text, ctx),
            Err(e) => self.show_error(e.to_string(), ctx),
        }
    }

    fn capture_mouse_position(&mut self) {
        self.spawn_position = self
            .platform
            .mouse_position()
            .map(|(x, y)| egui::pos2(x as f32, y as f32));
    }

    fn start_translation(&mut self, text: String, ctx: &egui::Context) {
        info!("starting translation ({} chars)", text.len());
        let _ = self.cmd_tx.send(WorkerCommand::Translate { text });
        self.state = OverlayState::Processing;
        self.capture_mouse_position();
        self.show_window(ctx);
    }

    fn show_error(&mut self, message: String, ctx: &egui::Context) {
        error!("pipeline error: {message}");
        self.state = OverlayState::Error(message);
        self.capture_mouse_position();
        self.show_window(ctx);
    }

    /// Reposition the window so it is centered on `spawn_position`, clamped to screen.
    fn reposition_window(&self, ctx: &egui::Context, win_size: egui::Vec2) {
        if let Some(cursor) = self.spawn_position {
            let mut x = cursor.x - win_size.x / 2.0;
            let mut y = cursor.y - win_size.y / 2.0;

            // Clamp to screen bounds so the overlay stays fully visible.
            if let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) {
                x = x.clamp(0.0, (monitor.x - win_size.x).max(0.0));
                y = y.clamp(0.0, (monitor.y - win_size.y).max(0.0));
            }

            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(x, y)));
        }
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        let win_size = ctx
            .input(|i| i.viewport().inner_rect)
            .map(|r| r.size())
            .unwrap_or(egui::vec2(480.0, 120.0));
        self.reposition_window(ctx, win_size);
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
                        info!("translation complete ({} chars), copied to clipboard", result.len());
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
            ..egui::Visuals::dark()
        });

        self.poll_responses(ctx);
        self.poll_hotkeys(ctx);

        let output = overlay::render(&self.state, ctx);

        // Resize viewport to fit content and re-center on spawn position.
        if let Some(desired) = output.desired_size {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));
            if !matches!(self.state, OverlayState::Hidden) {
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
