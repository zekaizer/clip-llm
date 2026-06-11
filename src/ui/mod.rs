mod overlay;
pub mod state_machine;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
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

/// A background selection-capture result tagged with the capture sequence id that
/// produced it, so stale captures (after a re-trigger / close) can be discarded.
type CaptureResult = (u64, Result<crate::ClipboardContent, crate::ClipboardError>);

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
    /// One-shot startup notice (e.g. a failed config load) surfaced via the
    /// error overlay on the first frame after the initial hide settles.
    startup_notice: Option<String>,
    /// Tap events from coordinator thread (hotkey detection runs off-UI).
    tap_rx: mpsc::Receiver<TapEvent>,
    /// Cached desired_size to avoid redundant send_viewport_cmd calls.
    last_desired_size: Option<egui::Vec2>,
    /// Whether the think block section is expanded in the Result state.
    think_expanded: bool,
    /// request_id whose Processing start time is currently tracked; a change
    /// restarts the elapsed-time clock so each new request counts from zero.
    processing_request_id: Option<u64>,
    /// When the current Processing request started, for the elapsed-time display.
    processing_started_at: Option<std::time::Instant>,
    /// Receives results from background selection-capture threads (double-tap).
    capture_rx: mpsc::Receiver<CaptureResult>,
    /// Sender cloned into each capture thread.
    capture_tx: mpsc::Sender<CaptureResult>,
    /// Monotonic capture id; a returning capture result is honored only if it
    /// matches the latest and the state machine is still Capturing.
    capture_seq: u64,
    /// Cancel flag for the in-flight capture thread; armed fresh per capture and
    /// set when the capture is superseded or aborted (cancel/close) so it stops
    /// before mutating the clipboard (clear + Cmd+C).
    capture_cancel: Arc<AtomicBool>,
    /// Transient mode-cycle preview while the cycle modifiers are held (Alt+Tab
    /// style). `None` outside a cycling gesture; the committed mode is always
    /// `sm.mode()`. Committed on modifier release.
    preview_mode: Option<crate::ProcessMode>,
    /// A capture deferred until the modifiers are released. For a double-tap the
    /// copy is simulated at commit; for a single-tap the clipboard was already
    /// read into `pending_content` and is processed at commit.
    pending_capture: bool,
    /// Pid of the frontmost app recorded at capture-trigger time, so the
    /// deferred copy simulation targets the source app even if the overlay
    /// takes focus before it runs (#55). `None` on platforms without
    /// per-process key posting.
    capture_target_pid: Option<i32>,
    /// What the in-flight capture thread is doing: Selection = copy simulation
    /// (double-tap), Clipboard = plain clipboard read (single-tap, #38).
    /// Decides how `poll_captures` routes the result.
    capture_kind: state_machine::CaptureSource,
    /// Single-tap only: the commit (modifier release) arrived while the
    /// background clipboard read was still in flight — process the content as
    /// soon as it lands instead of parking it in `pending_content`.
    single_commit_pending: bool,
    /// Single-tap clipboard content read at trigger time, shown in the picking
    /// overlay and processed on commit. `None` for the double-tap (selection)
    /// path, whose content only exists after release.
    pending_content: Option<crate::ClipboardContent>,
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
        let (capture_tx, capture_rx) = mpsc::channel();
        Self {
            sm: StateMachine::new(crate::ProcessMode::default()),
            cmd_tx,
            resp_rx,
            clipboard,
            platform: NativePlatform,
            spawn_position: None,
            initial_hide_done: false,
            startup_notice: None,
            tap_rx,
            last_desired_size: None,
            think_expanded: false,
            processing_request_id: None,
            processing_started_at: None,
            capture_rx,
            capture_tx,
            capture_seq: 0,
            capture_cancel: Arc::new(AtomicBool::new(false)),
            preview_mode: None,
            pending_capture: false,
            capture_target_pid: None,
            capture_kind: state_machine::CaptureSource::Selection,
            single_commit_pending: false,
            pending_content: None,
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
        let (capture_tx, capture_rx) = mpsc::channel();
        Self {
            sm: StateMachine::new(crate::ProcessMode::default()),
            cmd_tx,
            resp_rx,
            clipboard,
            platform: NativePlatform,
            spawn_position: None,
            initial_hide_done: false,
            startup_notice: None,
            tap_rx,
            last_desired_size: None,
            think_expanded: false,
            processing_request_id: None,
            processing_started_at: None,
            capture_rx,
            capture_tx,
            capture_seq: 0,
            capture_cancel: Arc::new(AtomicBool::new(false)),
            preview_mode: None,
            pending_capture: false,
            capture_target_pid: None,
            capture_kind: state_machine::CaptureSource::Selection,
            single_commit_pending: false,
            pending_content: None,
        }
    }
}

impl OverlayApp {
    /// Set a one-shot notice (e.g. "config ignored" from the startup config
    /// load) shown in the error overlay once the app is up.
    pub fn with_startup_notice(mut self, notice: Option<String>) -> Self {
        self.startup_notice = notice;
        self
    }

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
                            self.sm.handle(UiEvent::ClipboardError(friendly_clipboard_error(&e)));
                        // ClipboardError never emits WriteClipboard — recursion safe.
                        self.execute_effects(err_effects, ctx);
                        // Abort remaining effects: the state machine transitioned to
                        // Error, so subsequent effects (e.g. PasteClipboard) from the
                        // original chain are stale and must not execute.
                        return;
                    } else {
                        info!(
                            "{} complete ({} chars), copied to clipboard",
                            self.sm.mode().label(),
                            text.len()
                        );
                    }
                }
                UiEffect::ShowWindow => self.show_window(ctx),
                UiEffect::ShowWindowNoActivate => self.show_window_no_activate(ctx),
                UiEffect::StartCapture => {
                    // Defer the Cmd+C simulation until the cycle modifiers are
                    // released (handled in the CycleCommit tap arm). Copying while
                    // Ctrl+Shift are still held would send Cmd+Ctrl+Shift+C.
                    self.pending_capture = true;
                    // Record the source app NOW — the overlay may take focus
                    // before the deferred copy runs (e.g. the user grabs it to
                    // drag), and the copy must still target the source (#55).
                    self.capture_target_pid = self.platform.frontmost_app_pid();
                }
                UiEffect::HideWindow => {
                    ctx.memory_mut(|m| m.reset_areas());
                    self.hide_window(ctx);
                    self.spawn_position = None;
                    // Cancel any in-progress cycling gesture / deferred capture.
                    self.preview_mode = None;
                    self.pending_capture = false;
                    self.pending_content = None;
                    self.single_commit_pending = false;
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
                UiEffect::PasteClipboard => {
                    if let Err(e) = self.platform.paste_to_foreground() {
                        error!("paste simulation failed: {e}");
                    }
                }
                UiEffect::CancelCapture => {
                    // Tell the in-flight capture thread to stop before it mutates
                    // the clipboard; its (now stale) result is gated out separately.
                    self.capture_cancel.store(true, Ordering::SeqCst);
                    // A deferred (not-yet-started) capture is also abandoned.
                    self.pending_capture = false;
                    self.preview_mode = None;
                    self.pending_content = None;
                    self.single_commit_pending = false;
                }
            }
        }
    }

    // -- Tap action handling (from coordinator thread) --

    fn poll_tap_actions(&mut self, ctx: &egui::Context) {
        loop {
            let tap_event = match self.tap_rx.try_recv() {
                Ok(e) => e,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.fatal_disconnect(ctx, "coordinator");
                    break;
                }
            };
            // Set spawn_position from coordinator's first-press capture.
            // This runs before sm.handle() so CaptureMousePosition effect
            // (which skips if already set) preserves the first-press position.
            if let Some((x, y)) = tap_event.mouse_pos {
                self.spawn_position = Some(egui::pos2(x as f32, y as f32));
            }

            match tap_event.action {
                TapAction::SingleTap => {
                    info!("single-tap triggered, reading clipboard...");
                    // A fresh trigger ends any prior cycling preview.
                    self.preview_mode = None;
                    self.pending_content = None;
                    self.single_commit_pending = false;
                    // Show the picking overlay immediately and read the clipboard
                    // on a background thread (#38) — read_content() does
                    // synchronous pasteboard IPC plus PNG encoding, which can
                    // drop frames for large images. The content lands in
                    // pending_content (preview) or flows straight to processing
                    // if the commit already happened. CaptureStarted shows the
                    // overlay and sets pending_capture via StartCapture.
                    let effects = self.sm.handle(UiEvent::CaptureStarted {
                        source: state_machine::CaptureSource::Clipboard,
                    });
                    self.execute_effects(effects, ctx);
                    self.start_clipboard_read(ctx);
                }
                TapAction::DoubleTap => {
                    info!("double-tap triggered, capturing selection...");
                    self.preview_mode = None;
                    self.pending_content = None; // selection comes from copy-sim at commit
                    self.single_commit_pending = false;
                    // Show the picking overlay (spinner) immediately (non-activating).
                    // The actual copy is deferred (StartCapture -> pending_capture)
                    // until the modifiers are released, then started in CycleCommit.
                    let effects = self.sm.handle(UiEvent::CaptureStarted {
                        source: state_machine::CaptureSource::Selection,
                    });
                    self.execute_effects(effects, ctx);
                }
                TapAction::CycleAdvance => {
                    // Advance the preview to the next available mode (wrapping).
                    // Before content is known (double-tap capture in flight),
                    // cycle over all modes; once loaded, respect availability.
                    let available = self.sm.available_modes();
                    let targets: &[crate::ProcessMode] = if available.is_empty() {
                        crate::ProcessMode::ALL
                    } else {
                        available
                    };
                    let current = self.preview_mode.unwrap_or_else(|| self.sm.mode());
                    let next = current.next_available(targets);
                    if next != current {
                        self.preview_mode = Some(next);
                        info!("cycle preview: {}", next.label());
                    }
                }
                TapAction::CycleCommit { is_double_tap } => {
                    let preview = self.preview_mode.take();
                    let starting = self.pending_capture;
                    self.pending_capture = false;
                    info!(
                        "cycle commit: preview={:?}, is_double_tap={is_double_tap}",
                        preview.map(|m| m.label())
                    );
                    // Commit the chosen mode first so the deferred processing runs
                    // in it. In Capturing this only sets the mode (no effects).
                    if let Some(mode) = preview
                        && mode != self.sm.mode()
                    {
                        let effects = self.sm.handle(UiEvent::UserSwitchMode(mode));
                        self.execute_effects(effects, ctx);
                    }
                    // Run the deferred capture/processing now the modifiers are up.
                    if starting {
                        if is_double_tap {
                            // Selection copy is safe now (modifiers released).
                            self.start_capture(ctx);
                        } else if let Some(content) = self.pending_content.take() {
                            // Single-tap: process the clipboard content read at trigger.
                            let effects = self
                                .sm
                                .handle(UiEvent::ContentReady { content, auto_copy: false });
                            self.execute_effects(effects, ctx);
                        } else if matches!(self.sm.state(), OverlayState::Capturing) {
                            // Single-tap, but the background clipboard read has
                            // not landed yet (#38) — process it on arrival. The
                            // Capturing guard keeps a read that already failed
                            // (state moved to Error) from arming a dangling flag.
                            self.single_commit_pending = true;
                        }
                    }
                    self.pending_content = None;
                }
                TapAction::Pending => {}
            }
        }
    }

    // -- Background capture results (from start_capture threads) --

    fn poll_captures(&mut self, ctx: &egui::Context) {
        while let Ok((seq, result)) = self.capture_rx.try_recv() {
            // Discard stale captures: superseded by a newer trigger, or the user
            // already left Capturing (Escape / single-tap / re-trigger) before this
            // one finished. The seq check covers re-triggers; the state check covers
            // close/cancel.
            if seq != self.capture_seq || !matches!(self.sm.state(), OverlayState::Capturing) {
                continue;
            }
            // A cancelled/superseded capture is normally gated out above; ignore it
            // explicitly too so an abort never surfaces as a user-facing error.
            if matches!(result, Err(crate::ClipboardError::Cancelled)) {
                continue;
            }
            let event = match result {
                // Double-tap selection capture: commit already happened (the
                // copy only starts at commit), so process immediately.
                Ok(content) if self.capture_kind == state_machine::CaptureSource::Selection => {
                    UiEvent::ContentReady { content, auto_copy: true }
                }
                // Single-tap clipboard read (#38): if the commit already
                // arrived, process now; otherwise park the content for the
                // picking preview — CycleCommit takes it from pending_content.
                Ok(content) => {
                    if self.single_commit_pending {
                        self.single_commit_pending = false;
                        UiEvent::ContentReady { content, auto_copy: false }
                    } else {
                        self.pending_content = Some(content);
                        continue;
                    }
                }
                Err(e) => {
                    error!("clipboard capture failed: {e}");
                    self.single_commit_pending = false;
                    UiEvent::ClipboardError(friendly_clipboard_error(&e))
                }
            };
            let effects = self.sm.handle(event);
            self.execute_effects(effects, ctx);
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
        loop {
            let response = match self.resp_rx.try_recv() {
                Ok(r) => r,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.fatal_disconnect(ctx, "worker");
                    break;
                }
            };
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

    /// A background thread (worker or coordinator) dropped its channel sender,
    /// which only happens when that thread has exited (normally via panic). The
    /// app can no longer process LLM responses or hotkeys, so rather than linger
    /// as a silent zombie, log the cause and close the window to exit cleanly.
    fn fatal_disconnect(&self, ctx: &egui::Context, which: &str) {
        error!("{which} thread disconnected — clip-llm can no longer function, exiting");
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    // -- Focus handling --

    #[cfg(not(feature = "diagnostics"))]
    fn check_focus_lost(&mut self, ctx: &egui::Context) {
        if matches!(self.sm.state(), OverlayState::Hidden) {
            return;
        }
        let focused = ctx.input(|i| i.viewport().focused);
        if focused == Some(true) {
            // Execute the effects symmetrically with FocusLost. The list is empty
            // today, but discarding it silently would hide any future FocusGained
            // effect with no compiler warning.
            let effects = self.sm.handle(UiEvent::FocusGained);
            self.execute_effects(effects, ctx);
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
        if let Some(pos) = self.calculate_centered_position(win_size)
            && !self.platform.reposition_window(pos.x, pos.y)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
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

    /// Show the overlay during capture WITHOUT taking focus, so the user's app
    /// stays key and the simulated Cmd+C/Ctrl+C targets it. Positions at the
    /// spawn point (cursor) on first show; `update_viewport` re-centers once the
    /// rendered size is known.
    fn show_window_no_activate(&self, ctx: &egui::Context) {
        let pos = if self.sm.user_repositioned() {
            None
        } else {
            self.last_desired_size
                .and_then(|s| self.calculate_centered_position(s))
                .map(|p| (p.x, p.y))
                .or_else(|| self.spawn_position.map(|p| (p.x, p.y)))
        };

        if self.platform.show_window_no_activate(pos) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
    }

    /// Spawn a background thread to capture the current selection off the render
    /// thread (the 200 ms modifier-release wait + copy + clipboard poll would
    /// otherwise freeze the UI). Tagged with the current `capture_seq` so a stale
    /// result is discarded by `poll_captures`.
    fn start_capture(&mut self, ctx: &egui::Context) {
        self.capture_seq += 1;
        let seq = self.capture_seq;
        self.capture_kind = state_machine::CaptureSource::Selection;
        // Abort any previous in-flight capture, then arm a fresh flag for this one
        // so rapid re-triggers don't run overlapping clear()/Cmd+C in parallel.
        self.capture_cancel.store(true, Ordering::SeqCst);
        let cancel = Arc::new(AtomicBool::new(false));
        self.capture_cancel = cancel.clone();
        let tx = self.capture_tx.clone();
        let ctx = ctx.clone();
        let target = self.capture_target_pid;
        std::thread::spawn(move || {
            // Build a clipboard handle on this thread (avoids sharing the main
            // thread's, and sidesteps Send concerns). NativePlatform is a ZST.
            let result = match ClipboardManager::new() {
                Ok(mut cm) => cm.copy_and_read(&NativePlatform, &cancel, target),
                Err(e) => Err(e),
            };
            let _ = tx.send((seq, result));
            ctx.request_repaint();
        });
    }

    /// Spawn a background thread to read the clipboard for a single-tap (#38).
    /// `read_content()` does synchronous pasteboard IPC plus PNG encoding for
    /// images, which can drop frames on the render thread. Tagged with
    /// `capture_seq` like `start_capture` so stale results are discarded.
    fn start_clipboard_read(&mut self, ctx: &egui::Context) {
        self.capture_seq += 1;
        let seq = self.capture_seq;
        self.capture_kind = state_machine::CaptureSource::Clipboard;
        // Supersede any in-flight selection capture from a prior trigger.
        self.capture_cancel.store(true, Ordering::SeqCst);
        let tx = self.capture_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = match ClipboardManager::new() {
                Ok(mut cm) => cm.read_content(),
                Err(e) => Err(e),
            };
            let _ = tx.send((seq, result));
            ctx.request_repaint();
        });
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
            overlay::OverlayAction::PasteReplace => UiEvent::UserPaste,
            overlay::OverlayAction::TogglePin => UiEvent::UserTogglePin,
            overlay::OverlayAction::Retry => UiEvent::UserRetry,
        };
        let effects = self.sm.handle(event);
        self.execute_effects(effects, ctx);
    }

    /// Track how long the active Processing request has been running, keyed on its
    /// request_id so each new request restarts the clock. Returns the elapsed
    /// duration to display (None when not Processing).
    fn processing_elapsed(&mut self) -> Option<std::time::Duration> {
        if matches!(self.sm.state(), OverlayState::Processing) {
            let rid = self.sm.current_request_id();
            if self.processing_request_id != Some(rid) {
                self.processing_request_id = Some(rid);
                self.processing_started_at = Some(std::time::Instant::now());
            }
            self.processing_started_at.map(|t| t.elapsed())
        } else {
            self.processing_request_id = None;
            self.processing_started_at = None;
            None
        }
    }

    /// Request repaints: every frame while Processing/Capturing (spinner), idle poll in diagnostics mode.
    fn schedule_repaint(&self, ctx: &egui::Context) {
        if matches!(self.sm.state(), OverlayState::Processing | OverlayState::Capturing) {
            ctx.request_repaint();
        } else {
            #[cfg(feature = "diagnostics")]
            ctx.request_repaint_after(std::time::Duration::from_millis(IDLE_POLL_MS));
        }
    }
}

impl eframe::App for OverlayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let startup_settled = self.initial_hide_done;
        self.maybe_initial_hide(ctx);

        // Surface a pending startup notice via the error overlay, one frame
        // after the initial hide so the ShowWindow effect cannot race the
        // startup Visible(false) command.
        if startup_settled {
            if let Some(msg) = self.startup_notice.take() {
                self.capture_mouse_position();
                let effects = self.sm.handle(UiEvent::ClipboardError(msg));
                self.execute_effects(effects, ctx);
            }
        } else if self.startup_notice.is_some() {
            // Guarantee the follow-up frame; the event-driven repaint model
            // would otherwise sleep until the next external event.
            ctx.request_repaint();
        }

        self.poll_responses(ctx);
        self.poll_tap_actions(ctx);
        self.poll_captures(ctx);
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

        let elapsed = self.processing_elapsed();
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
            self.preview_mode,
            self.pending_content.as_ref().and_then(|c| c.text.as_deref()),
            self.sm.rephrase_params(),
            overlay::ThinkingState {
                mode: self.sm.effective_thinking_mode(),
                supported: self.sm.thinking_supported(),
            },
            self.sm.pinned(),
            self.sm.auto_copy(),
            self.sm.capture_source(),
            elapsed,
            ctx,
        );

        self.handle_overlay_action(ctx, output.action);
        self.update_viewport(ctx, output.desired_size);

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

        crate::platform::poll_tray_events(ctx);

        // Focus-loss auto-hide (skip during diagnostics).
        #[cfg(not(feature = "diagnostics"))]
        self.check_focus_lost(ctx);

        self.schedule_repaint(ctx);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}

/// Map a clipboard error to a short, user-facing message. The raw error is
/// logged separately for diagnostics, so no internal detail is lost.
fn friendly_clipboard_error(e: &crate::ClipboardError) -> String {
    use crate::ClipboardError::*;
    match e {
        NoTextInClipboard => "Clipboard is empty.".to_string(),
        NoTextAfterCopy => {
            "Could not capture selected text. Try selecting it again and double-tapping.".to_string()
        }
        EmptyCopy => "The selection contains no usable text or image.".to_string(),
        // A cancelled/superseded capture is gated out before display; this arm
        // exists only for exhaustiveness.
        Cancelled => "Capture cancelled.".to_string(),
        AccessFailed(_) | CopyFailed(_) => "Clipboard is unavailable.".to_string(),
        WriteFailed(_) => "Could not write to clipboard.".to_string(),
        ImageEncodeFailed(_) => "Could not process the clipboard image.".to_string(),
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
