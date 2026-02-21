mod overlay;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use tracing::{error, info};

use crate::clipboard::ClipboardManager;
use crate::hotkey::{HotkeyDetector, TapAction};
use crate::platform::NativePlatform;
use crate::worker::{WorkerCommand, WorkerResponse};

/// How long to show the result/error overlay before auto-closing.
const AUTO_CLOSE_SECS: f64 = 5.0;

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
    cmd_tx: mpsc::SyncSender<WorkerCommand>,
    resp_rx: mpsc::Receiver<WorkerResponse>,
    clipboard: ClipboardManager,
    platform: NativePlatform,
    detector: HotkeyDetector,
    result_shown_at: Option<Instant>,
}

impl OverlayApp {
    pub fn new(
        cmd_tx: mpsc::SyncSender<WorkerCommand>,
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
            result_shown_at: None,
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

    fn start_translation(&mut self, text: String, ctx: &egui::Context) {
        info!("starting translation ({} chars)", text.len());
        let _ = self.cmd_tx.send(WorkerCommand::Translate { text });
        self.state = OverlayState::Processing;
        self.result_shown_at = None;
        self.show_window(ctx);
    }

    fn show_error(&mut self, message: String, ctx: &egui::Context) {
        error!("pipeline error: {message}");
        self.state = OverlayState::Error(message);
        self.result_shown_at = Some(Instant::now());
        self.show_window(ctx);
    }

    fn show_window(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        self.state = OverlayState::Hidden;
        self.result_shown_at = None;
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
                    self.result_shown_at = Some(Instant::now());
                }
                WorkerResponse::Error { message } => {
                    error!("worker error: {message}");
                    self.state = OverlayState::Error(message);
                    self.result_shown_at = Some(Instant::now());
                }
            }
            // Ensure window is visible for result/error.
            self.show_window(ctx);
        }
    }

    fn check_auto_close(&mut self, ctx: &egui::Context) {
        if let Some(shown_at) = self.result_shown_at {
            if shown_at.elapsed().as_secs_f64() >= AUTO_CLOSE_SECS {
                self.hide_window(ctx);
            }
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

        let action = overlay::render(&self.state, ctx);
        match action {
            overlay::OverlayAction::None => {}
            overlay::OverlayAction::Close => {
                self.hide_window(ctx);
            }
            overlay::OverlayAction::Cancel => {
                let _ = self.cmd_tx.send(WorkerCommand::Cancel);
                self.hide_window(ctx);
            }
        }

        self.check_auto_close(ctx);

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
