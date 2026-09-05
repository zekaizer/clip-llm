mod overlay;
mod panel;
pub mod theme;
mod widgets;
pub mod state_machine;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use tokio::sync::mpsc as tokio_mpsc;

use eframe::egui;
use tracing::{debug, error, info, warn};

use crate::clipboard::ClipboardManager;
use crate::hotkey::{TapAction, TapEvent};
use crate::platform::{ModifierState, NativePlatform, Platform};
use crate::worker::{ProcessTask, WorkerCommand, WorkerResponse};

pub use state_machine::OverlayState;
use crate::config::PanelPlacement;
use state_machine::{StateMachine, UiEffect, UiEvent};

/// Polling interval for diagnostics scenario runner.
#[cfg(feature = "diagnostics")]
const IDLE_POLL_MS: u64 = 100;

/// A background selection-capture result tagged with the capture sequence id that
/// produced it, so stale captures (after a re-trigger / close) can be discarded.
type CaptureResult = (u64, Result<crate::ClipboardContent, crate::ClipboardError>);

/// Debounce window for parameter-change re-requests (#22): sweeping the
/// rephrase style/length or thinking pills coalesces into one LLM request
/// instead of firing a cancel + new round-trip per click.
const PARAM_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(300);

/// How long the copy button shows the ✓ confirmation after a copy (#16).
const COPY_CONFIRM: std::time::Duration = std::time::Duration::from_millis(1500);

/// Shorten a worker error message into a compact one-line label for the tray
/// "Last" status row (first sentence/line, capped).
fn tray_last_label(message: &str) -> String {
    let head = message.split(['\n', '.']).next().unwrap_or(message).trim();
    let head = if head.is_empty() { message } else { head };
    if head.chars().count() > 48 {
        format!("{}…", head.chars().take(47).collect::<String>())
    } else {
        head.to_string()
    }
}

/// Format a compact completion summary ("✓ 2.4s · 850 tokens") for the
/// Result bottom row — the same slot Processing's spinner+elapsed+Cancel row
/// occupies (see `TOP_ROW_HEIGHT`/`BOTTOM_ROW_HEIGHT` in overlay.rs), so it
/// fills what would otherwise be empty space left by those controls
/// disappearing. Token usage is often unavailable — not every server reports
/// it on streaming responses (see `DebugCapture::total_tokens`) — so it's
/// appended only when present. `None` when even the elapsed time isn't known
/// (nothing meaningful to show).
fn format_completion_status(debug: &crate::DebugCapture) -> Option<String> {
    let elapsed_ms = debug.elapsed_ms?;
    let secs = elapsed_ms as f32 / 1000.0;
    let mut out = String::new();
    if let Some(model) = debug.model.as_deref().map(short_model_label).filter(|m| !m.is_empty()) {
        out.push_str(&model);
        out.push_str(" \u{b7} ");
    }
    out.push_str(&format!("\u{2713} {secs:.1}s"));
    if let Some(tokens) = debug.total_tokens {
        out.push_str(&format!(" \u{b7} {tokens} tokens"));
    }
    Some(out)
}

/// Result of a tray-triggered config reload, for the tray Config row and the
/// overlay notice.
#[derive(Debug, Clone, PartialEq)]
enum ReloadOutcome {
    Applied {
        path: std::path::PathBuf,
        /// Startup-only settings that changed and still need a restart.
        restart_needed: Vec<&'static str>,
        /// The set of model profiles changed; the old clients stay in use.
        models_changed: bool,
    },
    Failed(&'static str),
}

/// One line describing a reload outcome.
fn format_reload_status(outcome: &ReloadOutcome) -> String {
    match outcome {
        ReloadOutcome::Applied { models_changed: true, .. } => {
            "Config: reloaded \u{2014} model profiles not rebuilt (restart to apply them)".to_string()
        }
        ReloadOutcome::Applied { restart_needed, .. } if !restart_needed.is_empty() => {
            format!("Config: reloaded \u{2014} restart to apply {}", restart_needed.join(", "))
        }
        ReloadOutcome::Applied { path, .. } => format!("Config: reloaded ({})", path.display()),
        ReloadOutcome::Failed(reason) => {
            format!("Config: reload failed ({reason}) \u{2014} keeping the previous settings")
        }
    }
}

/// Banner shown in the settings panel after a save.
fn settings_notice(outcome: &ReloadOutcome) -> String {
    match outcome {
        ReloadOutcome::Applied { models_changed: true, .. } => {
            "Saved. Restart to apply the model profiles.".to_string()
        }
        ReloadOutcome::Applied { restart_needed, .. } if !restart_needed.is_empty() => {
            format!("Saved. Restart to apply {}.", restart_needed.join(", "))
        }
        ReloadOutcome::Applied { .. } => "Saved and applied.".to_string(),
        ReloadOutcome::Failed(reason) => {
            format!("Saved, but reloading failed ({reason}) \u{2014} restart to apply.")
        }
    }
}

/// Status text for an automatic retry, shown in place of the Processing label.
fn format_retry_label(
    attempt: u32,
    max_attempts: u32,
    delay: std::time::Duration,
    rate_limited: bool,
) -> String {
    if rate_limited {
        // Ceil so a sub-second Retry-After never reads "in 0s".
        let secs = delay.as_millis().div_ceil(1000);
        format!("Rate limited \u{b7} retrying in {secs}s ({attempt}/{max_attempts})")
    } else {
        format!("Retrying ({attempt}/{max_attempts})\u{2026}")
    }
}

/// Longest model label the Result bottom row shows before eliding — the row
/// also holds the source badge, the timing summary and three action buttons
/// within `OVERLAY_WIDTH`.
const MODEL_LABEL_MAX_CHARS: usize = 24;

/// Remember a value the UI owns (`[ui].panel_size` / `panel_position` /
/// `zoom`; `None` removes the key). A failed write only costs the user that
/// preference on the next launch.
fn persist_ui_value(key: &str, value: Option<toml_edit::Value>) {
    match crate::settings::save_ui_value(key, value) {
        Ok(path) => debug!("[ui].{key} saved to {}", path.display()),
        Err(e) => warn!("[ui].{key} not saved: {e}"),
    }
}

/// `[ui].panel_size`, or the built-in default, clamped to the minimum.
fn panel_size_from_config() -> egui::Vec2 {
    crate::config::get()
        .ui_panel_size()
        .map_or(theme::size::DEFAULT_PANEL, |(w, h)| egui::vec2(w, h))
        .max(theme::size::MIN_PANEL)
}

/// Shorten a model id for the Result bottom row: drop the org namespace
/// (`MiniMaxAI/MiniMax-M2.5` → `MiniMax-M2.5`) and cap the length.
fn short_model_label(model: &str) -> String {
    let name = model.rsplit('/').next().unwrap_or(model).trim();
    if name.chars().count() <= MODEL_LABEL_MAX_CHARS {
        return name.to_string();
    }
    let mut out: String = name.chars().take(MODEL_LABEL_MAX_CHARS - 1).collect();
    out.push('\u{2026}');
    out
}

pub struct OverlayApp {
    sm: StateMachine,
    cmd_tx: tokio_mpsc::UnboundedSender<WorkerCommand>,
    resp_rx: mpsc::Receiver<WorkerResponse>,
    clipboard: ClipboardManager,
    platform: NativePlatform,
    /// Live Ctrl+Shift hold state from the process-lifetime OS watcher
    /// (spawned in `main.rs`), attached to each capture thread's
    /// `ClipboardManager` so `copy_and_read` can wait for an actual modifier
    /// release instead of a flat settle delay. See `start_capture`.
    modifier_state: ModifierState,
    /// Mouse cursor position captured at hotkey trigger time.
    spawn_position: Option<egui::Pos2>,
    /// Whether the startup hide has begun (the initial visibility command was sent).
    initial_hide_done: bool,
    /// Windows-only: set on the first frame after the startup `Visible(true)`
    /// sync. winit rebuilds the window exstyle on that visibility transition,
    /// so the taskbar exclusion must be (re)applied on the *next* frame, once
    /// that recomputation has happened. See [`OverlayApp::maybe_initial_hide`].
    pending_taskbar_exclude: bool,
    /// One-shot startup notice (e.g. a failed config load) surfaced via the
    /// error overlay on the first frame after the initial hide settles.
    startup_notice: Option<String>,
    /// Labels of the selectable model profiles in worker pool order; more than
    /// one makes the Result status label a "switch model" control.
    model_names: Vec<String>,
    /// Open settings panel (tray "Settings…"); it borrows the overlay window
    /// while the state machine stays Hidden.
    settings: Option<crate::settings::SettingsForm>,
    /// The form as opened / last saved; the panel's dirty state is form != baseline.
    settings_baseline: Option<crate::settings::SettingsForm>,
    /// In-flight "Test connection" (profile index, result channel).
    profile_test: Option<(usize, mpsc::Receiver<Result<String, String>>)>,
    /// Last finished connection test (profile index, outcome).
    profile_test_result: Option<(usize, Result<String, String>)>,
    /// Probed capabilities per profile index ("vision · thinking: …"), shown
    /// in the settings profile list. Cleared when the profile set changes.
    profile_caps: std::collections::HashMap<usize, String>,
    /// Tap events from coordinator thread (hotkey detection runs off-UI).
    tap_rx: mpsc::Receiver<TapEvent>,
    /// Cached desired_size to avoid redundant send_viewport_cmd calls.
    last_desired_size: Option<egui::Vec2>,
    /// Raw content size (pre shadow-padding) from the last rendered frame
    /// (diagnostics).
    last_content_size: Option<egui::Vec2>,
    /// Panel (frame incl. margin) size: `[ui].panel_size` at startup, then
    /// whatever the grip set (docs/UI-GUIDELINES.md §1).
    panel_size: egui::Vec2,
    /// `[ui].position`: center on the cursor, or reopen where last left.
    placement: PanelPlacement,
    /// Screen top-left to reopen at (`Remembered` only); refreshed on hide.
    remembered_pos: Option<egui::Pos2>,
    /// `[ui].zoom` as last written; compared on hide to persist keyboard zoom.
    saved_zoom: f32,
    /// Zoom factor seen by the last viewport pass — a change re-sends the
    /// window size, which eframe converts with the new pixels-per-point.
    last_zoom: f32,
    /// `[ui].zoom` has been applied to the context (first frame).
    zoom_applied: bool,
    /// The window position last actually applied (native reposition or
    /// `OuterPosition`), so repeated per-frame repositioning to the same spot
    /// is skipped. `send_viewport_cmd` internally triggers `request_repaint`,
    /// so resending an unchanged `OuterPosition` every frame while visible
    /// defeats the event-driven repaint model for Result/Error (CLAUDE.md
    /// repaint model). Reset on hide, on a fresh show, and whenever a new
    /// trigger updates `spawn_position`, so the next display still centers
    /// correctly.
    last_sent_pos: Option<egui::Pos2>,
    /// Screen top-left of the window when the current grip drag started;
    /// re-asserted after every size step (platforms anchor a programmatic
    /// resize at the center or bottom-left, which would slide the grip out
    /// from under the cursor). Lives for the drag gesture only — afterwards
    /// the user owns placement like after a window drag.
    resize_anchor: Option<egui::Pos2>,
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
    /// A worker request deferred by the parameter-change debounce (#22).
    /// Rapid pill clicks replace it; it is sent once the window expires. The
    /// state machine already shows Processing — only the send is delayed.
    pending_process: Option<(ProcessTask, std::time::Instant)>,
    /// When the user last copied via the 📋 button / Cmd+C — drives the ✓
    /// confirmation on the copy button for [`COPY_CONFIRM`] (#16a).
    copy_confirmed_at: Option<std::time::Instant>,
    /// Text of our last clipboard write and the change counter right after
    /// it. Lets `WriteClipboard` skip rewriting identical content that is
    /// still on the clipboard — the ↩ paste otherwise double-writes the
    /// result that auto-copy already placed (#16b).
    last_clipboard_write: Option<(String, u64)>,
    /// Raw request/response of the result currently shown, for the overlay's
    /// on-demand "copy debug" button. Set in poll_responses AFTER the worker
    /// Complete/Error effects run, so it survives the ResetAreas clear those
    /// transitions also emit. ResetAreas (and HideWindow) clear it, so a result
    /// shown WITHOUT a fresh worker response — a cached result, or a
    /// capture-read failure that never hit the server — leaves it cleared and
    /// the button hidden, never copying another request's data.
    last_debug: Option<crate::DebugCapture>,
    /// Completed-request tally for the tray Status menu: successes and errors
    /// seen so far this session. Drives the "Requests" / "Last" rows and the
    /// derived error rate.
    req_ok: u32,
    req_err: u32,
    /// Focus level seen on the previous frame. FocusGained/FocusLost are
    /// transition events to the state machine, so they must fire only on
    /// edges — re-firing FocusLost per unfocused frame hid the result the
    /// instant a detached request completed (#61). `None` while Hidden so
    /// the next show delivers a fresh edge.
    #[cfg(not(feature = "diagnostics"))]
    last_focused: Option<bool>,
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
        modifier_state: ModifierState,
        diag_action_rx: mpsc::Receiver<crate::diagnostics::ScenarioAction>,
        diag_state_tx: mpsc::Sender<&'static str>,
    ) -> Self {
        let (capture_tx, capture_rx) = mpsc::channel();
        Self {
            sm: StateMachine::new(crate::ProcessMode::initial()),
            cmd_tx,
            resp_rx,
            clipboard,
            platform: NativePlatform,
            modifier_state,
            spawn_position: None,
            initial_hide_done: false,
            pending_taskbar_exclude: false,
            startup_notice: None,
            model_names: Vec::new(),
            settings: None,
            settings_baseline: None,
            profile_test: None,
            profile_test_result: None,
            profile_caps: std::collections::HashMap::new(),
            tap_rx,
            last_desired_size: None,
            last_content_size: None,
            last_sent_pos: None,
            panel_size: panel_size_from_config(),
            placement: crate::config::get().ui_placement(),
            remembered_pos: crate::config::get().ui_panel_position().map(|(x, y)| egui::pos2(x, y)),
            saved_zoom: crate::config::get().ui_zoom(),
            last_zoom: 1.0,
            zoom_applied: false,
            resize_anchor: None,
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
            pending_process: None,
            copy_confirmed_at: None,
            last_clipboard_write: None,
            last_debug: None,
            req_ok: 0,
            req_err: 0,
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
        modifier_state: ModifierState,
    ) -> Self {
        let (capture_tx, capture_rx) = mpsc::channel();
        Self {
            sm: StateMachine::new(crate::ProcessMode::initial()),
            cmd_tx,
            resp_rx,
            clipboard,
            platform: NativePlatform,
            modifier_state,
            spawn_position: None,
            initial_hide_done: false,
            pending_taskbar_exclude: false,
            startup_notice: None,
            model_names: Vec::new(),
            settings: None,
            settings_baseline: None,
            profile_test: None,
            profile_test_result: None,
            profile_caps: std::collections::HashMap::new(),
            tap_rx,
            last_desired_size: None,
            last_content_size: None,
            last_sent_pos: None,
            panel_size: panel_size_from_config(),
            placement: crate::config::get().ui_placement(),
            remembered_pos: crate::config::get().ui_panel_position().map(|(x, y)| egui::pos2(x, y)),
            saved_zoom: crate::config::get().ui_zoom(),
            last_zoom: 1.0,
            zoom_applied: false,
            resize_anchor: None,
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
            pending_process: None,
            copy_confirmed_at: None,
            last_clipboard_write: None,
            last_debug: None,
            req_ok: 0,
            req_err: 0,
            last_focused: None,
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

    /// Labels of the model profiles the worker can switch between, pool order.
    pub fn with_model_names(mut self, names: Vec<String>) -> Self {
        self.model_names = names;
        self
    }

    /// Profile active at startup (`[ui].default_model`); the worker pool starts
    /// on the same index.
    pub fn with_active_model(mut self, index: usize) -> Self {
        self.sm.set_active_model(index);
        self
    }

    fn model_count(&self) -> usize {
        self.model_names.len().max(1)
    }

    /// State label for diagnostics: the settings panel is a pseudo-state on
    /// top of the (Hidden) state machine.
    #[cfg(feature = "diagnostics")]
    fn diag_state_name(&self) -> &'static str {
        if self.settings.is_some() {
            "Settings"
        } else {
            self.sm.variant_name()
        }
    }

    /// Announce a state change to the diagnostics collector and scenario
    /// runner (screenshot on settle).
    #[cfg(feature = "diagnostics")]
    fn diag_transition(&mut self, to: &'static str) {
        self.diag.on_state_transition(self.prev_state_name, to);
        self.prev_state_name = to;
        let _ = self.diag_state_tx.send(to);
    }

    // -- Settings panel --

    /// Tray "Settings…": show the panel in the overlay window. Any overlay
    /// session in progress is closed first — the panel takes the window.
    fn open_settings(&mut self, ctx: &egui::Context) {
        if self.settings.is_some() {
            // Already open: bring the window back rather than ignoring the
            // click — the panel may have ended up hidden (see below).
            if self.platform.show_window(None) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            }
            ctx.request_repaint();
            return;
        }
        if !matches!(self.sm.state(), OverlayState::Hidden) {
            // Close the session but keep the window: the panel takes it over.
            // `HideWindow` would queue `Visible(false)` for the end of this
            // frame, after the native show below — the window then vanished
            // with the panel "open" behind it (macOS applies viewport
            // commands at frame end, the native show runs now).
            let effects = self.sm_handle(UiEvent::UserClose);
            let effects = effects.into_iter().filter(|e| *e != UiEffect::HideWindow).collect();
            self.execute_effects(effects, ctx);
            self.preview_mode = None;
            self.pending_capture = false;
            self.spawn_position = None;
        }
        let active = self.model_names.get(self.sm.active_model()).map(String::as_str);
        let form = crate::settings::SettingsForm::from_config(&crate::config::get(), active);
        self.settings_baseline = Some(form.clone());
        self.settings = Some(form);
        self.profile_test = None;
        self.profile_test_result = None;
        self.capture_mouse_position();
        self.last_sent_pos = None;
        self.last_desired_size = None;
        // Centered on the cursor's monitor from an estimated size; the real
        // size lands on the first frame and only grows the window in place.
        let estimated = egui::vec2(theme::size::SETTINGS_WIDTH + theme::size::SHADOW_PAD * 2.0, 520.0);
        let pos = self.calculate_centered_position(estimated).map(|p| (p.x, p.y));
        if self.platform.show_window(pos) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
        #[cfg(feature = "diagnostics")]
        self.diag_transition("Settings");
        ctx.request_repaint();
    }

    fn close_settings(&mut self, ctx: &egui::Context) {
        self.settings_baseline = None;
        self.profile_test = None;
        self.profile_test_result = None;
        if self.settings.take().is_none() {
            return;
        }
        ctx.memory_mut(|m| m.reset_areas());
        self.hide_window(ctx);
        self.spawn_position = None;
        self.last_sent_pos = None;
        self.last_desired_size = None;
        #[cfg(feature = "diagnostics")]
        self.diag_transition("Hidden");
    }

    /// Validate, write the file, apply it (reload) and switch the model if the
    /// startup profile changed. The panel stays open and reports the outcome;
    /// errors stay next to the buttons.
    fn save_settings(&mut self, ctx: &egui::Context) {
        let Some(form) = self.settings.as_mut() else { return };
        let saved = form
            .to_patch()
            .and_then(|patch| crate::settings::save(&patch).map(|path| (patch, path)));
        let (patch, path) = match saved {
            Err(msg) => {
                form.error = Some(msg);
                form.notice = None;
                return;
            }
            Ok(ok) => ok,
        };
        info!("settings saved to {}", path.display());
        let outcome = self.perform_reload();
        crate::platform::update_tray_config(&format_reload_status(&outcome));
        if let Some(index) = patch
            .default_model
            .as_deref()
            .and_then(|name| self.model_names.iter().position(|n| n == name))
        {
            self.select_model(ctx, index);
        }
        if let Some(form) = self.settings.as_mut() {
            form.error = None;
            form.notice = Some(settings_notice(&outcome));
            self.settings_baseline = Some(form.clone());
        }
    }

    /// Render the settings panel for this frame and act on its result.
    fn render_settings_panel(&mut self, ctx: &egui::Context) -> overlay::OverlayOutput {
        let path = crate::config::candidate_path().map(|p| p.display().to_string());
        // Collect a finished connection test before rendering.
        if let Some((index, rx)) = &self.profile_test {
            match rx.try_recv() {
                Ok(result) => {
                    self.profile_test_result = Some((*index, result));
                    self.profile_test = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.profile_test_result = Some((*index, Err("test thread died".into())));
                    self.profile_test = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let running = self.profile_test.as_ref().map(|(i, _)| *i);
        let result = self.profile_test_result.as_ref();
        // Capabilities are probed per worker-pool index; the form lists specs
        // in config order, which matches the pool only while every profile
        // built — look up by name to stay correct otherwise.
        let caps_by_name: std::collections::HashMap<&str, &str> = self
            .profile_caps
            .iter()
            .filter_map(|(i, text)| self.model_names.get(*i).map(|n| (n.as_str(), text.as_str())))
            .collect();
        let test = |i: usize| -> overlay::ProfileTestView {
            if running == Some(i) {
                overlay::ProfileTestView::Running
            } else {
                match result {
                    Some((idx, r)) if *idx == i => overlay::ProfileTestView::Done(r),
                    _ => overlay::ProfileTestView::Idle,
                }
            }
        };
        let Some(form) = self.settings.as_mut() else {
            return overlay::OverlayOutput { action: overlay::OverlayAction::None, desired_size: None, content_size: None };
        };
        let caps = |name: &str| caps_by_name.get(name).map(|c| (*c).to_string());
        let (action, output) = overlay::render_settings(
            ctx,
            form,
            self.settings_baseline.as_ref(),
            path.as_deref(),
            test,
            caps,
        );
        match action {
            overlay::SettingsAction::None => {}
            overlay::SettingsAction::StartDrag => ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag),
            overlay::SettingsAction::Cancel => self.close_settings(ctx),
            overlay::SettingsAction::OpenConfig => crate::platform::open_config_file(),
            overlay::SettingsAction::Save => self.save_settings(ctx),
            overlay::SettingsAction::TestProfile(index) => self.start_profile_test(ctx, index),
        }
        output
    }

    /// Tray "Reload Config": re-read the file, swap it in, rebuild the model
    /// clients when the profile set is unchanged, and report via the tray
    /// Config row (plus the overlay notice on failure).
    fn reload_config(&mut self, ctx: &egui::Context) {
        let outcome = self.perform_reload();
        let line = format_reload_status(&outcome);
        info!("{line}");
        crate::platform::update_tray_config(&line);
        if matches!(outcome, ReloadOutcome::Failed(_)) {
            // Failure needs attention now; success is visible in the tray row.
            self.capture_mouse_position();
            let effects = self.sm_handle(UiEvent::ClipboardError(line));
            self.execute_effects(effects, ctx);
        }
    }

    /// "Test connection" for the profile being edited: build a client from the
    /// form's current values and run one tiny request on a helper thread.
    fn start_profile_test(&mut self, ctx: &egui::Context, index: usize) {
        let Some(profile) = self.settings.as_ref().and_then(|f| f.profiles.get(index)) else { return };
        let spec = match profile.to_spec() {
            Ok(spec) => spec,
            Err(msg) => {
                self.profile_test_result = Some((index, Err(msg)));
                return;
            }
        };
        let (tx, rx) = mpsc::channel();
        self.profile_test = Some((index, rx));
        self.profile_test_result = None;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|rt| {
                    rt.block_on(async {
                        let client = crate::api::client::LlmClient::for_spec(&spec).map_err(|e| e.to_string())?;
                        client.test_connection().await.map_err(|e| crate::worker::friendly_api_error(&e))
                    })
                });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    /// Re-read the file, swap it in and rebuild the model clients — also when
    /// the profile set changed: the worker pool, tray submenu and the active
    /// selection (kept by name) follow. Pure of UI; callers report the outcome.
    fn perform_reload(&mut self) -> ReloadOutcome {
        let previous = crate::config::get();
        match crate::config::reload() {
            Err(reason) => ReloadOutcome::Failed(reason),
            Ok(path) => {
                let current = crate::config::get();
                let restart_needed = previous.restart_required_changes(&current);
                let rebuilt = current
                    .model_specs()
                    .map_err(crate::ApiError::InvalidConfig)
                    .and_then(|specs| crate::api::client::build_profiles(&specs));
                let models_changed = match rebuilt {
                    Ok(set) => {
                        self.apply_profile_set(set);
                        false
                    }
                    Err(e) => {
                        warn!("config reload: model profiles not rebuilt: {e}");
                        true
                    }
                };
                ReloadOutcome::Applied { path, restart_needed, models_changed }
            }
        }
    }

    /// Swap in freshly built profiles: worker clients, tray submenu, and the
    /// active selection (same profile by name, else the first).
    fn apply_profile_set(&mut self, set: crate::api::client::ProfileSet) {
        let labels = set.labels();
        let active_name = self.model_names.get(self.sm.active_model()).cloned();
        let active = active_name
            .as_deref()
            .and_then(|name| labels.iter().position(|l| l == name))
            .unwrap_or(0);
        if labels != self.model_names {
            info!("model profiles now {:?} (active #{active})", labels);
        }
        let tray: Vec<crate::platform::TrayModel> = labels
            .iter()
            .map(|label| crate::platform::TrayModel { label: label.clone(), unavailable: None })
            .chain(set.unavailable.iter().map(|(label, why)| crate::platform::TrayModel {
                label: label.clone(),
                unavailable: Some(why.clone()),
            }))
            .collect();
        let _ = self.cmd_tx.send(WorkerCommand::ReloadClients(set.clients));
        let _ = self.cmd_tx.send(WorkerCommand::SelectModel(active));
        self.profile_caps.clear();
        self.model_names = labels;
        self.sm.set_active_model(active);
        crate::platform::update_tray_models(&tray, active);
    }

    /// Apply a model-profile choice from the tray or the overlay.
    fn select_model(&mut self, ctx: &egui::Context, index: usize) {
        let effects = self.sm_handle(UiEvent::UserSelectModel(index));
        self.execute_effects(effects, ctx);
    }

    // -- State machine dispatch --

    /// Dispatch an event to the state machine. The sole entry point into
    /// `StateMachine::handle` from the adapter.
    fn sm_handle(&mut self, event: UiEvent) -> Vec<UiEffect> {
        self.sm.handle(event)
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
                    // An immediate request supersedes any debounce-parked one
                    // (#22) — flushing the stale task later would queue a
                    // wasted LLM round-trip behind this request.
                    self.pending_process = None;
                    self.dispatch_process(ProcessTask {
                        content,
                        mode,
                        rephrase_params,
                        thinking_mode,
                        request_id,
                    });
                }
                UiEffect::SendCancel => {
                    debug!("ui: cancel in-flight request");
                    let _ = self.cmd_tx.send(WorkerCommand::Cancel);
                }
                UiEffect::SelectModel(index) => {
                    info!("ui: select model profile #{index}");
                    let _ = self.cmd_tx.send(WorkerCommand::SelectModel(index));
                    crate::platform::update_tray_model(index);
                }
                UiEffect::WriteClipboard(text) => {
                    // Skip rewriting identical content that is still on the
                    // clipboard (#16b): the ↩ paste otherwise double-writes the
                    // result auto-copy already placed, polluting clipboard
                    // managers. If anything else touched the clipboard since
                    // (change counter moved), write again so the button still
                    // delivers the result it promises.
                    let counter = self.platform.clipboard_change_count();
                    let already_ours = self
                        .last_clipboard_write
                        .as_ref()
                        .is_some_and(|(t, c)| *t == text && *c == counter);
                    if already_ours {
                        info!("clipboard already holds this text; skipping rewrite");
                    } else if let Err(e) = self.clipboard.write_text(&text) {
                        error!("clipboard write failed: {e}");
                        let err_effects =
                            self.sm_handle(UiEvent::ClipboardError(friendly_clipboard_error(&e)));
                        // ClipboardError never emits WriteClipboard — recursion safe.
                        self.execute_effects(err_effects, ctx);
                        // Abort remaining effects: the state machine transitioned to
                        // Error, so subsequent effects (e.g. PasteClipboard) from the
                        // original chain are stale and must not execute.
                        return;
                    } else {
                        self.last_clipboard_write =
                            Some((text.clone(), self.platform.clipboard_change_count()));
                        info!(
                            "{} complete ({} chars), copied to clipboard",
                            self.sm.mode().label(),
                            text.len()
                        );
                    }
                }
                UiEffect::ShowWindow => {
                    // A fresh show applies its own initial position natively
                    // (below); invalidate the reposition cache so the first
                    // subsequent per-frame reposition_window call isn't gated
                    // out by a position left over from a previous display.
                    self.last_sent_pos = None;
                    self.show_window(ctx);
                }
                UiEffect::ShowWindowNoActivate => {
                    self.last_sent_pos = None;
                    self.show_window_no_activate(ctx);
                }
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
                    #[cfg(feature = "diagnostics")]
                    {
                        // Not a captured transition (nothing to screenshot),
                        // but the chain must show the window was hidden so
                        // the next show is not judged as an in-place move.
                        self.prev_state_name = "Hidden";
                        let _ = self.diag_state_tx.send("Hidden");
                    }
                    ctx.memory_mut(|m| m.reset_areas());
                    self.remember_position(ctx);
                    self.persist_zoom(ctx);
                    self.hide_window(ctx);
                    self.spawn_position = None;
                    self.last_sent_pos = None;
                    // Cancel any in-progress cycling gesture / deferred capture,
                    // and drop a debounce-parked request (overlay is closing).
                    self.preview_mode = None;
                    self.pending_capture = false;
                    self.pending_content = None;
                    self.single_commit_pending = false;
                    self.pending_process = None;
                    self.last_debug = None;
                }
                UiEffect::CaptureMousePosition => self.capture_mouse_position(),
                UiEffect::ResetAreas => {
                    #[cfg(feature = "diagnostics")]
                    {
                        let to = self.sm.variant_name();
                        self.diag_transition(to);
                    }
                    self.think_expanded = false;
                    // Clear the debug snapshot on every area reset. For a worker
                    // result this fires before poll_responses re-sets last_debug
                    // (same frame, after effects); for a cached result or capture
                    // failure nothing re-sets it, so the button stays hidden
                    // rather than copying a prior request's data.
                    self.last_debug = None;
                    // Always reset the Area geometry, including at a latched
                    // Processing->Result/Error transition. The latch is a
                    // FLOOR (never render shorter), not an exact pin: a
                    // Result taller than the last Processing frame must grow
                    // to its natural size, and that natural size is only
                    // measured correctly from a fresh Area sizing pass — the
                    // carried-over max_rect otherwise starves the ScrollArea
                    // and the window crawls toward the natural height over
                    // several frames (~+33px/frame, measured) instead of
                    // resizing once. A same-size transition re-measures to
                    // the identical geometry, so the reset costs nothing
                    // there.
                    ctx.memory_mut(|m| m.reset_areas());
                    // `reset_areas` only clears Area geometry — egui persists
                    // each ScrollArea's offset (and stick-to-bottom flag)
                    // separately in `ctx.data`, keyed by widget id, for the
                    // whole process lifetime. Without clearing it here, a
                    // result the user once scrolled reopens at that stale
                    // offset on every later request in the same mode instead
                    // of at the top. ResetAreas fires only on real state
                    // transitions (never mid-stream or on think/param
                    // toggles), so this cannot fight in-state scrolling.
                    clear_scroll_state(ctx);
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
            // A hotkey takes the window back from the settings panel; unsaved
            // edits are dropped (the panel is a modal-free side view).
            if self.settings.take().is_some() {
                ctx.memory_mut(|m| m.reset_areas());
                self.last_desired_size = None;
            }
            // Set spawn_position from coordinator's first-press capture.
            // This runs before sm.handle() so CaptureMousePosition effect
            // (which skips if already set) preserves the first-press position.
            if let Some((x, y)) = tap_event.mouse_pos {
                self.spawn_position = Some(egui::pos2(x as f32, y as f32));
                // A new trigger's spawn point invalidates any cached
                // reposition target — a re-trigger that skips HideWindow
                // (e.g. re-double-tap while Result is still shown) must
                // still reposition to the new cursor location.
                self.last_sent_pos = None;
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
                    let effects = self.sm_handle(UiEvent::CaptureStarted {
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
                    // Invalidate any capture still in flight from a previous trigger
                    // (a slow single-tap clipboard read, or a previous double-tap's
                    // copy simulation) right now. This gesture's own capture is
                    // deferred to CycleCommit (below), so capture_seq/capture_kind
                    // would otherwise stay unchanged while CaptureStarted moves the
                    // state to Capturing — letting the old thread's late result pass
                    // poll_captures' `seq == capture_seq` gate and land in
                    // pending_content under this gesture's Selection badge.
                    self.capture_cancel.store(true, Ordering::SeqCst);
                    self.capture_seq += 1;
                    // Show the picking overlay (spinner) immediately (non-activating).
                    // The actual copy is deferred (StartCapture -> pending_capture)
                    // until the modifiers are released, then started in CycleCommit.
                    let effects = self.sm_handle(UiEvent::CaptureStarted {
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
                        crate::ProcessMode::display_order()
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
                        let effects = self.sm_handle(UiEvent::UserSwitchMode(mode));
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
            let effects = self.sm_handle(event);
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
                    self.sm_handle(UiEvent::UserSwitchMode(mode));
                    let effects = self.sm_handle(UiEvent::ContentReady {
                        content: crate::ClipboardContent::text_only(text),
                        auto_copy: true,
                    });
                    self.execute_effects(effects, ctx);
                }
                crate::diagnostics::ScenarioAction::SwitchMode(mode) => {
                    let effects = self.sm_handle(UiEvent::UserSwitchMode(mode));
                    self.execute_effects(effects, ctx);
                }
                crate::diagnostics::ScenarioAction::HideOverlay => {
                    let effects = self.sm_handle(UiEvent::UserClose);
                    self.execute_effects(effects, ctx);
                }
                crate::diagnostics::ScenarioAction::OpenSettings => self.open_settings(ctx),
                crate::diagnostics::ScenarioAction::EditSettingsSample => {
                    if let Some(form) = self.settings.as_mut() {
                        form.secondary = "Klingon".into();
                        form.default_mode = crate::ProcessMode::Summarize;
                        form.single_tap_pinned = true;
                        if let Some(t) = form.thinking.iter_mut().find(|(m, _)| *m == crate::ProcessMode::Summarize) {
                            t.1 = Some(crate::ThinkingMode::Think);
                        }
                        form.error = Some("Double-tap window must be 100\u{2013}2000 ms.".into());
                        form.editing = Some(1.min(form.profiles.len().saturating_sub(1)));
                        self.profile_test_result =
                            Some((1, Ok("Connected in 0.8s \u{b7} \"OK\"".into())));
                        ctx.memory_mut(|m| m.reset_areas());
                        self.diag_transition("SettingsEdited");
                    }
                }
                crate::diagnostics::ScenarioAction::CloseSettings => self.close_settings(ctx),
                crate::diagnostics::ScenarioAction::BeginCapture { text } => {
                    let effects = self.sm_handle(UiEvent::CaptureStarted {
                        source: state_machine::CaptureSource::Clipboard,
                    });
                    self.execute_effects(effects, ctx);
                    // No real capture runs: stand in for the clipboard read
                    // that would have parked its content here.
                    self.pending_capture = false;
                    self.pending_content = Some(crate::ClipboardContent::text_only(text));
                }
                crate::diagnostics::ScenarioAction::Resize(w, h) => {
                    // Same path as a real grip drag (anchor + state machine event).
                    self.handle_overlay_action(ctx, overlay::OverlayAction::Resize(egui::vec2(w, h)));
                    self.diag_transition("Resized");
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

    /// Record a completed request's outcome: bump the success/error tally,
    /// log it, and refresh the tray Status rows. `last` is a short label for
    /// the "Last" row (e.g. "ok" or a trimmed error message).
    fn record_request_outcome(&mut self, ok: bool, last: &str) {
        if ok {
            self.req_ok += 1;
        } else {
            self.req_err += 1;
        }
        let total = self.req_ok + self.req_err;
        let rate = (self.req_err * 100).checked_div(total).unwrap_or(0);
        if ok {
            info!(
                "request ok ({}✓/{}✗, {rate}% err)",
                self.req_ok, self.req_err
            );
        } else {
            warn!(
                "request error: {last} ({}✓/{}✗, {rate}% err)",
                self.req_ok, self.req_err
            );
        }
        crate::platform::update_tray_requests(self.req_ok, self.req_err, last);
    }

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
            // Raw request/response of a finished request, held aside so it is
            // stored AFTER the effects run. The Processing→Result/Error effects
            // include ResetAreas, which clears last_debug; setting it before
            // execute_effects would wipe it in the same frame, before render —
            // so the copy-debug button would never appear.
            let mut fresh_debug: Option<crate::DebugCapture> = None;
            // A request cancelled just before it completed can still deliver its
            // Complete/Error here after a newer request has already started (the
            // state machine gates the *state* transition on this same check, but
            // the tally and debug snapshot live outside it, in the adapter). Skip
            // them so a stale request's outcome/debug data never overwrites the
            // one for the request currently on screen.
            let current_request_id = self.sm.current_request_id();
            let event = match response {
                WorkerResponse::Complete { result, think_content, request_id, incomplete, debug } => {
                    if request_id == current_request_id {
                        self.record_request_outcome(true, if incomplete.is_some() { "partial" } else { "ok" });
                        fresh_debug = Some(debug);
                    }
                    UiEvent::WorkerResult {
                        text: result,
                        think_content,
                        request_id,
                        incomplete,
                    }
                }
                WorkerResponse::Error { message, request_id, debug } => {
                    if request_id == current_request_id {
                        self.record_request_outcome(false, &tray_last_label(&message));
                        fresh_debug = Some(debug);
                    }
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
                WorkerResponse::Retrying { request_id, attempt, max_attempts, delay, rate_limited } => {
                    UiEvent::RetryScheduled {
                        request_id,
                        label: format_retry_label(attempt, max_attempts, delay, rate_limited),
                    }
                }
                WorkerResponse::ProbeComplete { vision_supported, thinking_method, model_index } => {
                    use crate::api::client::ThinkingControlMethod as Tcm;
                    self.profile_caps.insert(
                        model_index,
                        format!(
                            "vision {} \u{b7} thinking control: {}",
                            if vision_supported { "\u{2713}" } else { "\u{2717}" },
                            thinking_method.key()
                        ),
                    );
                    // A slow probe of a profile the user already left must not
                    // describe the current one.
                    if model_index != self.sm.active_model() {
                        debug!("ui: dropping stale probe result for model profile #{model_index}");
                        continue;
                    }
                    let thinking_label = match thinking_method {
                        Tcm::ChatTemplateKwargs => "kwargs",
                        Tcm::SystemPromptTag => "prompt tag",
                        Tcm::ReasoningEffort => "reasoning effort",
                        Tcm::Unsupported => "unsupported",
                    };
                    crate::platform::update_tray_probe(vision_supported, thinking_label);
                    UiEvent::ThinkingProbeResult(thinking_method != Tcm::Unsupported)
                }
            };
            let effects = self.sm_handle(event);
            self.execute_effects(effects, ctx);
            // Store after effects: this result's debug survives the ResetAreas
            // clear, while a cached result or capture failure (which don't set
            // fresh_debug) correctly leaves last_debug cleared → button hidden.
            if fresh_debug.is_some() {
                self.last_debug = fresh_debug;
            }
        }

        // Refresh the tray "Telemetry" row with the latest shipped/dropped
        // counts (only when the remote sink is enabled). Runs on UI activity —
        // the shipping itself is independent and immediate.
        if crate::config::get().telemetry_url().is_some() {
            let (shipped, dropped) = crate::telemetry::telemetry_counts();
            crate::platform::update_tray_telemetry(shipped, dropped);
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
            // Forget the focus level so the next show delivers a fresh edge.
            self.last_focused = None;
            return;
        }
        // Fire only on focus *transitions*. The state machine counts
        // focus → unfocus cycles; re-firing FocusLost on every unfocused
        // frame hid the result the moment a detached request completed (#61).
        let focused = ctx.input(|i| i.viewport().focused);
        if focused == self.last_focused {
            return;
        }
        self.last_focused = focused;
        if focused == Some(true) {
            // Execute the effects symmetrically with FocusLost. The list is empty
            // today, but discarding it silently would hide any future FocusGained
            // effect with no compiler warning.
            let effects = self.sm_handle(UiEvent::FocusGained);
            self.execute_effects(effects, ctx);
        } else if focused == Some(false) {
            let effects = self.sm_handle(UiEvent::FocusLost);
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
    /// `win_size` is in egui logical points; the returned top-left is in
    /// platform screen coordinates (Quartz points on macOS, physical pixels
    /// on Windows — see `Platform::mouse_position`).
    fn calculate_centered_position(&self, win_size: egui::Vec2) -> Option<egui::Pos2> {
        let cursor = self.spawn_position?;
        let (cx, cy) = (cursor.x as f64, cursor.y as f64);
        let bounds = self.platform.display_bounds_at_point(cx, cy);
        let scale = self.platform.points_to_screen_scale_at(cx, cy) as f32;
        Some(centered_position_screen(cursor, win_size, scale, bounds))
    }

    /// Position for this frame's reposition pass (`win_size` in OS points —
    /// see `os_window_size`): centered on the spawn point, except during a
    /// grip drag, which keeps the top-left the drag started from, or with
    /// `Remembered` placement, which reopens at the stored top-left. Anchors
    /// are clamped only if the panel would leave the display work area.
    fn calculate_target_position(&self, win_size: egui::Vec2) -> Option<egui::Pos2> {
        let anchor = self.resize_anchor.or(match self.placement {
            PanelPlacement::Remembered => self.remembered_pos,
            PanelPlacement::Cursor => None,
        });
        if let Some(anchor) = anchor {
            let (bounds, scale) = self.spawn_bounds_and_scale();
            return Some(anchored_position_screen(anchor, win_size, scale, bounds));
        }
        self.calculate_centered_position(win_size)
    }

    /// Display work area and points→screen scale at the spawn point. No spawn
    /// point → no bounds (anchors pass through unclamped) and the scale is moot.
    fn spawn_bounds_and_scale(&self) -> (Option<(f64, f64, f64, f64)>, f32) {
        match self.spawn_position {
            Some(c) => (
                self.platform.display_bounds_at_point(c.x as f64, c.y as f64),
                self.platform.points_to_screen_scale_at(c.x as f64, c.y as f64) as f32,
            ),
            None => (None, 1.0),
        }
    }

    /// The window's current top-left in the screen units `reposition_window`
    /// takes: the last position this adapter applied while it still owned
    /// placement, otherwise (the user dragged the window since) the viewport
    /// rect egui reports, scaled from points.
    fn current_top_left_screen(&self, ctx: &egui::Context) -> Option<egui::Pos2> {
        if !self.sm.user_repositioned()
            && let Some(pos) = self.last_sent_pos
        {
            return Some(pos);
        }
        // egui points → OS points is the zoom factor; → screen units the
        // platform scale (1 on macOS, the DPI factor on Windows).
        let (_, scale) = self.spawn_bounds_and_scale();
        let zoom = ctx.zoom_factor();
        ctx.input(|i| i.viewport().outer_rect).map(|r| (r.min.to_vec2() * zoom * scale).to_pos2())
    }

    /// Window size in egui points for the current panel size (frame plus
    /// shadow pad) — what `ViewportCommand::InnerSize` takes.
    fn window_size(&self) -> egui::Vec2 {
        self.panel_size.max(theme::size::MIN_PANEL) + egui::Vec2::splat(theme::size::SHADOW_PAD * 2.0)
    }

    /// The same in OS points — what positioning math takes: eframe scales
    /// `InnerSize` by the zoom factor when it reaches the window.
    fn os_window_size(&self, ctx: &egui::Context) -> egui::Vec2 {
        self.window_size() * ctx.zoom_factor()
    }

    /// `Remembered` placement: store where the window is (before it hides)
    /// for the next trigger and the next launch.
    fn remember_position(&mut self, ctx: &egui::Context) {
        if self.placement != PanelPlacement::Remembered {
            return;
        }
        let Some(pos) = self.current_top_left_screen(ctx) else { return };
        if self.remembered_pos.is_some_and(|p| (p - pos).length() < 1.0) {
            return;
        }
        self.remembered_pos = Some(pos);
        persist_ui_value("panel_position", Some(crate::settings::point_pair(pos.x, pos.y)));
    }

    /// Persist a zoom the user changed with Cmd/Ctrl +/−/0 (egui handles the
    /// keys); called on hide so a session's zoom survives a restart.
    fn persist_zoom(&mut self, ctx: &egui::Context) {
        let zoom = ctx.zoom_factor();
        if (zoom - self.saved_zoom).abs() < 1e-3 {
            return;
        }
        self.saved_zoom = zoom;
        let rounded = f64::from((zoom * 100.0).round() / 100.0);
        persist_ui_value("zoom", Some(toml_edit::Value::from(rounded)));
    }

    /// Apply a new panel size, anchoring the window's top-left for the
    /// resize (see `resize_anchor`).
    fn set_panel_size(&mut self, ctx: &egui::Context, size: egui::Vec2) {
        if self.resize_anchor.is_none() {
            self.resize_anchor = self.current_top_left_screen(ctx);
        }
        self.panel_size = size.max(theme::size::MIN_PANEL);
    }

    /// Reposition the window while the overlay is already visible (e.g. after size change).
    ///
    /// Delegates to the platform for native DPI-safe repositioning (e.g. Windows SetWindowPos
    /// bypasses winit's per-monitor scaling). Falls back to ViewportCommand::OuterPosition.
    ///
    /// Gated on `last_sent_pos`: this runs every frame while visible, and both the
    /// native path and `OuterPosition` would otherwise repeat pointlessly each
    /// frame — on macOS `OuterPosition` also triggers `request_repaint`
    /// internally, which would defeat the event-driven repaint model for
    /// Result/Error (CLAUDE.md repaint model) by busy-looping forever.
    fn reposition_window(&mut self, ctx: &egui::Context, win_size: egui::Vec2) {
        let Some(pos) = self.calculate_target_position(win_size) else { return };
        if self.last_sent_pos == Some(pos) {
            return;
        }
        self.last_sent_pos = Some(pos);
        debug!("viewport: OuterPosition {pos:?} in {:?}", self.sm.state().variant_name());
        if !self.platform.reposition_window(pos.x, pos.y) {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
        }
    }

    fn show_window(&self, ctx: &egui::Context) {
        // Skip repositioning if user has manually dragged the window;
        // only reposition on initial show (before any drag).
        let pos = if self.sm.user_repositioned() {
            None
        } else {
            self.calculate_target_position(self.os_window_size(ctx)).map(|p| (p.x, p.y))
        };

        if self.platform.show_window(pos) {
            // Windows: sync winit to visible=true to maintain ControlFlow::Wait (egui#5229).
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
    }

    /// Show the overlay during capture WITHOUT taking focus, so the user's app
    /// stays key and the simulated Cmd+C/Ctrl+C targets it.
    fn show_window_no_activate(&self, ctx: &egui::Context) {
        let pos = if self.sm.user_repositioned() {
            None
        } else {
            self.calculate_target_position(self.os_window_size(ctx)).map(|p| (p.x, p.y))
        };

        if self.platform.show_window_no_activate(pos) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        }
    }

    /// Spawn a background thread to capture the current selection off the render
    /// thread (the modifier-release wait + copy + clipboard poll would
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
        // Cloned into the capture thread so `copy_and_read` can wait for an
        // actual Ctrl+Shift release instead of a flat settle delay (this path
        // is only reached via CycleCommit, which the coordinator already gates
        // on an observed release — see `coordinator::resolve_trigger` — so the
        // wait is usually near-instant).
        let modifier_state = self.modifier_state.clone();
        std::thread::spawn(move || {
            // Build a clipboard handle on this thread (avoids sharing the main
            // thread's, and sidesteps Send concerns). NativePlatform is a ZST.
            let result = match ClipboardManager::new() {
                Ok(cm) => cm
                    .with_modifier_state(modifier_state)
                    .copy_and_read(&NativePlatform, &cancel, target),
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
    ///
    /// On Windows this also defuses winit's taskbar-icon race. winit rebuilds the
    /// full window exstyle — re-adding `WS_EX_APPWINDOW`, dropping the
    /// `WS_EX_TOOLWINDOW` we set — every time its internal `VISIBLE` flag
    /// *transitions*. The app starts `with_visible(false)`, so that transition
    /// would otherwise fire on the first focus-show (`Visible(true)`) and re-add
    /// the taskbar icon. Instead we force the transition here, while the window is
    /// parked offscreen, then apply the exclusion on the *next* frame — after
    /// winit's recomputation. From then on the app only uses
    /// `move_window_offscreen` (never `Visible(false)`), so winit's `VISIBLE` flag
    /// never changes again, no further exstyle recomputation occurs, and the
    /// exclusion is genuinely permanent.
    fn maybe_initial_hide(&mut self, ctx: &egui::Context) {
        // Windows step 2: winit has applied the startup `Visible(true)` and
        // rebuilt the exstyle. Clear `WS_EX_APPWINDOW` / set `WS_EX_TOOLWINDOW`
        // now — once, for good.
        if self.pending_taskbar_exclude {
            self.pending_taskbar_exclude = false;
            self.platform.exclude_from_taskbar();
            return;
        }
        if self.initial_hide_done {
            return;
        }
        self.initial_hide_done = true;
        if self.platform.hide_window() {
            // Windows: the window is now parked offscreen. Sync winit's `VISIBLE`
            // flag to true so its one-time exstyle recomputation happens here
            // (offscreen, invisible) rather than on the first focus-show. The
            // exclusion is applied next frame, after that recomputation lands.
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            self.pending_taskbar_exclude = true;
            ctx.request_repaint();
        } else {
            // macOS / others: hidden via `Visible(false)`; no exstyle race, and
            // `exclude_from_taskbar` is a no-op there.
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.platform.exclude_from_taskbar();
        }
    }

    /// Resize the viewport when the desired content size changes, then reposition.
    fn update_viewport(
        &mut self,
        ctx: &egui::Context,
        desired: Option<egui::Vec2>,
        content_size: Option<egui::Vec2>,
    ) {
        self.last_content_size = content_size;
        let Some(size) = desired else { return };
        let zoom = ctx.zoom_factor();
        if zoom != self.last_zoom {
            // The window's pixel size follows egui points × zoom, so the same
            // point size must be re-sent after a zoom change.
            self.last_zoom = zoom;
            self.last_desired_size = None;
        }
        if self.last_desired_size != Some(size) {
            self.last_desired_size = Some(size);
            // Every window resize is a potential whole-window flash on macOS
            // (the wgpu surface is reconfigured), so each one is logged.
            debug!("viewport: InnerSize {size:?} in {:?}", self.sm.state().variant_name());
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            // A programmatic resize can itself move the OS window (platforms
            // anchor it at the center or bottom-left rather than the top-left)
            // — invalidate the reposition gate so `reposition_window` below
            // re-asserts the anchored position this same frame, even though
            // the *computed* target position hasn't changed.
            if self.resize_anchor.is_some() {
                self.last_sent_pos = None;
            }
        } else {
            // The anchor lives while the size is changing: one settled frame
            // ends the gesture, so a later trigger centers afresh.
            self.resize_anchor = None;
        }
        // A user drag hands placement to the OS for good — except that a
        // user resize (which also sets `user_repositioned`) must keep
        // re-asserting its anchor, see `resize_anchor`.
        let owns_placement = !self.sm.user_repositioned() || self.resize_anchor.is_some();
        if !matches!(self.sm.state(), OverlayState::Hidden) && owns_placement {
            self.reposition_window(ctx, size * zoom);
        }
    }

    /// Translate the overlay action returned by `render()` into state machine events.
    fn handle_overlay_action(&mut self, ctx: &egui::Context, action: overlay::OverlayAction) {
        // Parameter pills (#22): the state machine applies the change at once
        // (instant visual feedback, Processing state), but the resulting
        // SendProcess is debounced so sweeping pills coalesces into one request.
        let debounce = matches!(
            action,
            overlay::OverlayAction::ChangeRephraseStyle(_)
                | overlay::OverlayAction::ChangeRephraseLength(_)
                | overlay::OverlayAction::ChangeThinkingMode(_)
        );
        let event = match action {
            overlay::OverlayAction::None => return,
            overlay::OverlayAction::Close => UiEvent::UserClose,
            overlay::OverlayAction::Cancel => UiEvent::UserCancel,
            overlay::OverlayAction::StartDrag => {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                UiEvent::UserStartDrag
            }
            overlay::OverlayAction::Resize(size) => {
                self.set_panel_size(ctx, size);
                UiEvent::UserResize
            }
            overlay::OverlayAction::ResetSize => {
                self.set_panel_size(ctx, theme::size::DEFAULT_PANEL);
                persist_ui_value("panel_size", None);
                UiEvent::UserResize
            }
            overlay::OverlayAction::ResizeDone => {
                let size = crate::settings::point_pair(self.panel_size.x, self.panel_size.y);
                persist_ui_value("panel_size", Some(size));
                return;
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
            overlay::OverlayAction::CycleModel => {
                let next = (self.sm.active_model() + 1) % self.model_count();
                self.select_model(ctx, next);
                return;
            }
            overlay::OverlayAction::CopyDebug => {
                // Side channel, not a state-machine event: write the raw
                // request/response snapshot straight to the clipboard. Bypasses
                // the WriteClipboard dedup so it never disturbs result tracking.
                if let Some(debug) = &self.last_debug {
                    let text = debug.to_clipboard_text();
                    match self.clipboard.write_text(&text) {
                        Ok(()) => info!("copied debug snapshot to clipboard ({} chars)", text.len()),
                        Err(e) => error!("debug clipboard write failed: {e}"),
                    }
                }
                return;
            }
        };
        let effects = self.sm_handle(event);
        if debounce {
            self.execute_effects_debounced(effects, ctx);
        } else {
            self.execute_effects(effects, ctx);
        }

        // Copy confirmation (#16a): flip the 📋 button to ✓ for a moment.
        // Gated on Result — UserCopy is a no-op in any other state.
        if matches!(action, overlay::OverlayAction::CopyToClipboard)
            && matches!(self.sm.state(), OverlayState::Result(_))
        {
            self.copy_confirmed_at = Some(std::time::Instant::now());
            ctx.request_repaint_after(COPY_CONFIRM);
        }
    }

    /// Execute effects, deferring any `SendProcess` into `pending_process`
    /// (parameter-change debounce, #22). Everything else — notably the
    /// `SendCancel` that aborts the in-flight request — runs immediately.
    fn execute_effects_debounced(&mut self, effects: Vec<UiEffect>, ctx: &egui::Context) {
        let mut immediate = Vec::with_capacity(effects.len());
        for effect in effects {
            match effect {
                UiEffect::SendProcess {
                    content,
                    mode,
                    rephrase_params,
                    thinking_mode,
                    request_id,
                } => {
                    self.pending_process = Some((
                        ProcessTask { content, mode, rephrase_params, thinking_mode, request_id },
                        std::time::Instant::now() + PARAM_DEBOUNCE,
                    ));
                    // Guarantee a frame after the deadline (the Processing
                    // state repaints every frame anyway; this is a safety net).
                    ctx.request_repaint_after(PARAM_DEBOUNCE);
                }
                other => immediate.push(other),
            }
        }
        self.execute_effects(immediate, ctx);
    }

    /// Send a debounced request once its window expires (called every frame).
    fn flush_pending_process(&mut self, ctx: &egui::Context) {
        let Some(&(_, deadline)) = self.pending_process.as_ref() else { return };
        let now = std::time::Instant::now();
        if now >= deadline {
            let (task, _) = self.pending_process.take().expect("checked above");
            self.dispatch_process(task);
        } else {
            ctx.request_repaint_after(deadline - now);
        }
    }

    /// Dispatch a `ProcessTask` to the worker thread, logging its shape
    /// (request_id, mode, content size) for observability. Shared by the
    /// immediate `SendProcess` effect and the parameter-change debounce flush
    /// (#22), so every worker dispatch is logged the same way regardless of
    /// which path sent it.
    fn dispatch_process(&self, task: ProcessTask) {
        let text_len = task.content.text.as_ref().map_or(0, |t| t.len());
        let img_count = task.content.images.len();
        info!(
            request_id = task.request_id,
            mode = task.mode.label(),
            text_len,
            img_count,
            "ui: dispatch request to worker"
        );
        let _ = self.cmd_tx.send(WorkerCommand::Process(task));
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
        theme::color::apply(ctx);
        if !self.zoom_applied {
            self.zoom_applied = true;
            ctx.set_zoom_factor(self.saved_zoom);
        }
        let startup_settled = self.initial_hide_done;
        self.maybe_initial_hide(ctx);

        // Surface a pending startup notice via the error overlay, one frame
        // after the initial hide so the ShowWindow effect cannot race the
        // startup Visible(false) command.
        if startup_settled {
            if let Some(msg) = self.startup_notice.take() {
                self.capture_mouse_position();
                let effects = self.sm_handle(UiEvent::ClipboardError(msg));
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
        self.flush_pending_process(ctx);

        // Expire the copy ✓ confirmation (#16a).
        if let Some(at) = self.copy_confirmed_at {
            let elapsed = at.elapsed();
            if elapsed >= COPY_CONFIRM {
                self.copy_confirmed_at = None;
            } else {
                ctx.request_repaint_after(COPY_CONFIRM - elapsed);
            }
        }
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

        let output = if self.settings.is_some() && matches!(self.sm.state(), OverlayState::Hidden) {
            // The settings panel borrows the window while the state machine
            // stays Hidden; it has no overlay action to translate.
            self.render_settings_panel(ctx)
        } else {
            let elapsed = self.processing_elapsed();
            let mut output = overlay::render(
                self.sm.state(),
                self.sm.mode(),
                overlay::StreamingState {
                    text: self.sm.streaming_text(),
                    think_started: self.sm.think_started(),
                    retry_notice: self.sm.retry_notice(),
                    think_content: self.sm.think_content(),
                    think_expanded: self.think_expanded,
                    incomplete: self.sm.result_incomplete(),
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
                self.sm.content_files(),
                self.copy_confirmed_at.is_some(),
                elapsed,
                self.last_debug.is_some(),
                self.last_debug.as_ref().and_then(format_completion_status),
                self.model_count() > 1,
                self.panel_size,
                ctx,
            );
            let action = std::mem::replace(&mut output.action, overlay::OverlayAction::None);
            self.handle_overlay_action(ctx, action);
            output
        };
        self.update_viewport(ctx, output.desired_size, output.content_size);

        // Diagnostics: record frame data + flush stale screenshots.
        #[cfg(feature = "diagnostics")]
        {
            use crate::diagnostics::FrameSnapshot;
            self.diag.record_frame(FrameSnapshot {
                frame: self.diag.frame_counter(),
                state: self.diag_state_name(),
                mode: self.sm.mode().label(),
                content_size: output.content_size.map(|v| [v.x, v.y]),
                desired_size: output.desired_size.map(|v| [v.x, v.y]),
                viewport_inner_rect: ctx
                    .input(|i| i.viewport().inner_rect)
                    .map(|r| [r.min.x, r.min.y, r.max.x, r.max.y]),
                spawn_position: self.spawn_position.map(|p| [p.x, p.y]),
                user_repositioned: self.sm.user_repositioned(),
                panel_size: [self.panel_size.x, self.panel_size.y],
            });
            self.diag.tick_screenshot(ctx);
            self.diag.flush_pending_if_stale();
        }

        match crate::platform::poll_tray_events(ctx) {
            Some(crate::platform::TrayAction::SelectModel(index)) => self.select_model(ctx, index),
            Some(crate::platform::TrayAction::ReloadConfig) => self.reload_config(ctx),
            Some(crate::platform::TrayAction::OpenSettings) => self.open_settings(ctx),
            None => {}
        }

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
        UnsupportedFiles(names) => format!(
            "Unsupported file type: {}. Text and PNG files can be sent.",
            list_names(names)
        ),
        FileTooLarge { name, limit_bytes } => format!(
            "{name} is too large to send (limit {} MiB).",
            limit_bytes / (1024 * 1024)
        ),
        FileReadFailed { name, .. } => format!("Could not read {name}."),
    }
}

/// Join file names for a message, eliding past the first three.
fn list_names(names: &[String]) -> String {
    const SHOWN: usize = 3;
    let shown = names.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    if names.len() > SHOWN {
        format!("{shown} (+{} more)", names.len() - SHOWN)
    } else {
        shown
    }
}

/// Clamp a top-left position so the window of `win_size` stays fully within
/// `bounds`. `bounds` is `(origin_x, origin_y, width, height)` in the same
/// coordinate space as `pos`. Shared clamping logic for both
/// `center_clamped_to_bounds` and `anchored_clamped_to_bounds`.
fn clamp_top_left_to_bounds(
    pos: egui::Pos2,
    win_size: egui::Vec2,
    bounds: Option<(f64, f64, f64, f64)>,
) -> egui::Pos2 {
    let mut x = pos.x;
    let mut y = pos.y;

    if let Some((ox, oy, w, h)) = bounds {
        let (ox, oy, w, h) = (ox as f32, oy as f32, w as f32, h as f32);
        x = x.clamp(ox, (ox + w - win_size.x).max(ox));
        y = y.clamp(oy, (oy + h - win_size.y).max(oy));
    }

    egui::pos2(x, y)
}

/// Drop every persisted `ScrollArea` state (scroll offset + stick-to-bottom
/// flag) so the next frame's scroll areas start from the top. egui stores this
/// per widget id in `ctx.data` for the process lifetime — `Memory::reset_areas`
/// does not touch it. Called from the `ResetAreas` effect, i.e. only at real
/// state transitions.
fn clear_scroll_state(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove_by_type::<egui::scroll_area::State>());
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
    let center = egui::pos2(cursor.x - win_size.x / 2.0, cursor.y - win_size.y / 2.0);
    clamp_top_left_to_bounds(center, win_size, bounds)
}

/// Keep a fixed top-left `anchor` in place, clamping it within `bounds` only
/// when the window would otherwise overflow the display work area — shifting
/// it up/left just enough to fit, never past the opposite edge (so a window
/// taller than the work area pins to the top rather than centering).
///
/// `bounds` has the same shape as in `center_clamped_to_bounds`. Used to keep
/// the overlay's top edge stationary across a session's height changes
/// (`OverlayApp::calculate_anchored_position`), instead of re-centering the
/// whole window on every resize — the fix for the Processing→Result
/// resize-jump defect (window visibly moving on the final answer's height
/// delta).
fn anchored_clamped_to_bounds(
    anchor_top_left: egui::Pos2,
    win_size: egui::Vec2,
    bounds: Option<(f64, f64, f64, f64)>,
) -> egui::Pos2 {
    clamp_top_left_to_bounds(anchor_top_left, win_size, bounds)
}

/// Center a window of `win_size_points` (egui logical points) on `cursor` and
/// clamp within `bounds`, where `cursor` and `bounds` are platform screen
/// coordinates (Quartz points on macOS, physical virtual-screen pixels on
/// Windows — see `Platform::mouse_position`). `scale` converts points to
/// screen units on the target monitor (`Platform::points_to_screen_scale_at`);
/// centering/clamping with the unscaled point size on a non-100% monitor
/// would misplace the window and misjudge whether it fits the work area.
fn centered_position_screen(
    cursor: egui::Pos2,
    win_size_points: egui::Vec2,
    scale: f32,
    bounds: Option<(f64, f64, f64, f64)>,
) -> egui::Pos2 {
    center_clamped_to_bounds(cursor, win_size_points * scale, bounds)
}

/// Anchored variant of `centered_position_screen` — same unit contract, but
/// keeps a fixed top-left instead of centering (see `anchored_clamped_to_bounds`).
fn anchored_position_screen(
    anchor_top_left: egui::Pos2,
    win_size_points: egui::Vec2,
    scale: f32,
    bounds: Option<(f64, f64, f64, f64)>,
) -> egui::Pos2 {
    anchored_clamped_to_bounds(anchor_top_left, win_size_points * scale, bounds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(ox: f64, oy: f64, w: f64, h: f64) -> Option<(f64, f64, f64, f64)> {
        Some((ox, oy, w, h))
    }

    // --- scroll state reset (stale result scroll offset — see clear_scroll_state) ---

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn clear_scroll_state_drops_persisted_offsets() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("result_scroll_regression");
        let mut state = egui::scroll_area::State::default();
        state.offset = egui::vec2(0.0, 123.0);
        state.store(&ctx, id);
        assert!(
            egui::scroll_area::State::load(&ctx, id).is_some(),
            "precondition: scroll state must persist in ctx.data"
        );

        clear_scroll_state(&ctx);

        assert!(
            egui::scroll_area::State::load(&ctx, id).is_none(),
            "clear_scroll_state must drop every persisted ScrollArea state"
        );
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

    // --- anchored_clamped_to_bounds: mirrors the center_clamped_to_bounds suite
    // above, but the input is already a top-left corner (an anchor recorded
    // from a prior centering pass), not a cursor to center on. ---

    #[test]
    fn anchored_no_bounds_keeps_anchor() {
        let pos = anchored_clamped_to_bounds(egui::pos2(800.0, 350.0), egui::vec2(400.0, 300.0), None);
        assert_eq!(pos, egui::pos2(800.0, 350.0));
    }

    #[test]
    fn anchored_fits_stays_put() {
        // Anchor well inside the work area and the new (taller) size still fits
        // → top-left is unchanged, only the bottom edge would move.
        let pos = anchored_clamped_to_bounds(
            egui::pos2(980.0, 520.0),
            egui::vec2(600.0, 500.0),
            bounds(0.0, 0.0, 2560.0, 1440.0),
        );
        assert_eq!(pos, egui::pos2(980.0, 520.0));
    }

    #[test]
    fn anchored_bottom_overflow_shifts_up() {
        // Anchor near the bottom edge; growing taller would overflow the work
        // area, so the top-left shifts up just enough to keep the bottom in
        // bounds (the x coordinate is untouched).
        let pos = anchored_clamped_to_bounds(
            egui::pos2(980.0, 1200.0),
            egui::vec2(600.0, 400.0),
            bounds(0.0, 0.0, 2560.0, 1440.0),
        );
        // max_y = 0 + 1440 - 400 = 1040
        assert_eq!(pos, egui::pos2(980.0, 1040.0));
    }

    #[test]
    fn anchored_right_overflow_shifts_left() {
        let pos = anchored_clamped_to_bounds(
            egui::pos2(2200.0, 520.0),
            egui::vec2(600.0, 400.0),
            bounds(0.0, 0.0, 2560.0, 1440.0),
        );
        // max_x = 0 + 2560 - 600 = 1960
        assert_eq!(pos, egui::pos2(1960.0, 520.0));
    }

    #[test]
    fn anchored_taller_than_work_area_pins_to_top() {
        // A window taller than the work area must pin to the top of the work
        // area, never centering or overflowing above it.
        let pos = anchored_clamped_to_bounds(
            egui::pos2(980.0, 600.0),
            egui::vec2(600.0, 2000.0),
            bounds(0.0, 0.0, 2560.0, 1440.0),
        );
        assert_eq!(pos, egui::pos2(980.0, 0.0));
    }

    #[test]
    fn anchored_never_shifts_above_top_of_work_area() {
        // Anchor already at the very top; even a tall window must not move
        // above origin y.
        let pos = anchored_clamped_to_bounds(
            egui::pos2(980.0, 0.0),
            egui::vec2(600.0, 400.0),
            bounds(0.0, 0.0, 2560.0, 1440.0),
        );
        assert_eq!(pos, egui::pos2(980.0, 0.0));
    }

    #[test]
    fn anchored_secondary_monitor_fits_no_clamp() {
        // Anchor on a secondary monitor with an offset origin; the taller
        // window still fits within that monitor's own work area.
        let pos = anchored_clamped_to_bounds(
            egui::pos2(3200.0, 300.0),
            egui::vec2(600.0, 500.0),
            bounds(2560.0, 0.0, 1920.0, 1080.0),
        );
        assert_eq!(pos, egui::pos2(3200.0, 300.0));
    }

    #[test]
    fn anchored_secondary_monitor_bottom_overflow_shifts_up() {
        let pos = anchored_clamped_to_bounds(
            egui::pos2(3200.0, 900.0),
            egui::vec2(600.0, 400.0),
            bounds(2560.0, 0.0, 1920.0, 1080.0),
        );
        // max_y = 0 + 1080 - 400 = 680
        assert_eq!(pos, egui::pos2(3200.0, 680.0));
    }

    #[test]
    fn anchored_negative_origin_monitor_no_clamp() {
        // Secondary monitor to the left of primary (negative x origin); anchor
        // and size both fit within its work area.
        let pos = anchored_clamped_to_bounds(
            egui::pos2(-1260.0, 340.0),
            egui::vec2(600.0, 400.0),
            bounds(-1920.0, 0.0, 1920.0, 1080.0),
        );
        assert_eq!(pos, egui::pos2(-1260.0, 340.0));
    }

    // --- mixed-DPI screen-space positioning: win_size arrives in egui logical
    // points and must be converted into the target monitor's screen units
    // (physical pixels on Windows) before centering/clamping. Scenario:
    // primary 1920×1080 @ 100% (physical 0..1920), secondary 1920×1080
    // logical @ 150% (physical 1920..4800, i.e. 2880×1620 physical). ---

    #[test]
    fn mixed_dpi_centers_with_scaled_window_size() {
        let pos = centered_position_screen(
            egui::pos2(3000.0, 800.0), // cursor, physical px on the 150% monitor
            egui::vec2(400.0, 300.0),  // window size, egui points
            1.5,
            bounds(1920.0, 0.0, 2880.0, 1620.0),
        );
        // screen-space size 600×450 → top-left (3000-300, 800-225)
        assert_eq!(pos, egui::pos2(2700.0, 575.0));
    }

    #[test]
    fn mixed_dpi_clamps_with_scaled_window_size() {
        let pos = centered_position_screen(
            egui::pos2(4700.0, 800.0),
            egui::vec2(400.0, 300.0),
            1.5,
            bounds(1920.0, 0.0, 2880.0, 1620.0),
        );
        // max_x = 1920 + 2880 - 600 (scaled width, not the 400pt raw width)
        assert_eq!(pos.x, 4200.0);
    }

    #[test]
    fn mixed_dpi_anchored_overflow_shifts_up_with_scaled_size() {
        let pos = anchored_position_screen(
            egui::pos2(2000.0, 1400.0),
            egui::vec2(400.0, 300.0),
            1.5,
            bounds(1920.0, 0.0, 2880.0, 1620.0),
        );
        // scaled height 450 → max_y = 1620 - 450 = 1170
        assert_eq!(pos, egui::pos2(2000.0, 1170.0));
    }

    #[test]
    fn unit_scale_reduces_to_plain_point_space_centering() {
        // macOS (and any 100% monitor): scale 1.0 must behave exactly like the
        // unscaled point-space centering.
        let pos = centered_position_screen(
            egui::pos2(1280.0, 720.0),
            egui::vec2(600.0, 400.0),
            1.0,
            bounds(0.0, 0.0, 2560.0, 1440.0),
        );
        assert_eq!(pos, egui::pos2(980.0, 520.0));
    }

    // --- poll_responses: stale worker responses must not pollute adapter state ---

    /// Build a minimal `OverlayApp` for adapter-level tests, returning the app
    /// plus the sender counterpart of its worker-response channel so tests can
    /// inject `WorkerResponse`s as if the worker thread had sent them.
    ///
    /// Constructing a real `ClipboardManager` opens a handle onto the actual
    /// system clipboard, shared process-wide with `clipboard::tests` — callers
    /// must hold `crate::clipboard::test_support::lock_clipboard()` for the
    /// duration of the test to avoid racing those.
    fn new_test_app() -> (OverlayApp, mpsc::Sender<WorkerResponse>) {
        let (cmd_tx, _cmd_rx) = tokio_mpsc::unbounded_channel();
        let (resp_tx, resp_rx) = mpsc::channel();
        let (_tap_tx, tap_rx) = mpsc::channel();
        let clipboard = ClipboardManager::new().expect("clipboard manager");
        // No real OS watcher in tests — a default ModifierState is permanently
        // inactive, so `copy_and_read` falls back to the fixed settle delay.
        let modifier_state = ModifierState::default();
        #[cfg(feature = "diagnostics")]
        let app = {
            let (_diag_action_tx, diag_action_rx) = mpsc::channel();
            let (diag_state_tx, _diag_state_rx) = mpsc::channel();
            OverlayApp::new(
                cmd_tx,
                resp_rx,
                clipboard,
                tap_rx,
                modifier_state,
                diag_action_rx,
                diag_state_tx,
            )
        };
        #[cfg(not(feature = "diagnostics"))]
        let app = OverlayApp::new(cmd_tx, resp_rx, clipboard, tap_rx, modifier_state);
        (app, resp_tx)
    }

    /// A request cancelled just before its (late) `Complete` arrives must not
    /// have that stale outcome tallied into `req_ok`/`req_err`, nor its debug
    /// snapshot surfaced via `last_debug` — both belong to whichever request
    /// is current, not to one the state machine has already moved past.
    #[test]
    fn poll_responses_ignores_stale_completion_tally_and_debug() {
        let _lock = crate::clipboard::test_support::lock_clipboard();
        let (mut app, resp_tx) = new_test_app();
        let ctx = egui::Context::default();

        // Start request 1 (Processing), then cancel it (state machine keeps
        // current_request_id unchanged — reset_to_hidden does not touch it)
        // and start request 2, which becomes current.
        app.sm.handle(UiEvent::ContentReady {
            content: crate::ClipboardContent::text_only("first".into()),
            auto_copy: true,
        });
        let stale_id = app.sm.current_request_id();
        app.sm.handle(UiEvent::UserCancel);
        app.sm.handle(UiEvent::ContentReady {
            content: crate::ClipboardContent::text_only("second".into()),
            auto_copy: true,
        });
        let current_id = app.sm.current_request_id();
        assert_ne!(stale_id, current_id, "test setup must produce two distinct request ids");

        // The cancelled request's completion arrives late.
        resp_tx
            .send(WorkerResponse::Complete {
                result: "stale answer".into(),
                think_content: None,
                request_id: stale_id,
                incomplete: None,
                debug: crate::DebugCapture {
                    endpoint: Some("http://stale".into()),
                    ..Default::default()
                },
            })
            .unwrap();
        app.poll_responses(&ctx);

        assert_eq!(app.req_ok, 0, "stale completion must not be tallied as a success");
        assert_eq!(app.req_err, 0);
        assert!(app.last_debug.is_none(), "stale completion's debug snapshot must not surface");
    }

    /// Same as above but for `WorkerResponse::Error`: a stale failure must not
    /// bump `req_err` or surface its debug snapshot either.
    #[test]
    fn poll_responses_ignores_stale_error_tally_and_debug() {
        let _lock = crate::clipboard::test_support::lock_clipboard();
        let (mut app, resp_tx) = new_test_app();
        let ctx = egui::Context::default();

        app.sm.handle(UiEvent::ContentReady {
            content: crate::ClipboardContent::text_only("first".into()),
            auto_copy: true,
        });
        let stale_id = app.sm.current_request_id();
        app.sm.handle(UiEvent::UserCancel);
        app.sm.handle(UiEvent::ContentReady {
            content: crate::ClipboardContent::text_only("second".into()),
            auto_copy: true,
        });
        let current_id = app.sm.current_request_id();
        assert_ne!(stale_id, current_id, "test setup must produce two distinct request ids");

        resp_tx
            .send(WorkerResponse::Error {
                message: "stale error".into(),
                request_id: stale_id,
                debug: crate::DebugCapture {
                    endpoint: Some("http://stale".into()),
                    ..Default::default()
                },
            })
            .unwrap();
        app.poll_responses(&ctx);

        assert_eq!(app.req_ok, 0);
        assert_eq!(app.req_err, 0, "stale error must not be tallied");
        assert!(app.last_debug.is_none(), "stale error's debug snapshot must not surface");
    }

    /// Sanity check that the gate is genuinely id-based, not a blanket
    /// suppression: a `Complete` matching the current request must still be
    /// tallied and surfaced normally.
    #[test]
    fn poll_responses_records_current_completion_tally_and_debug() {
        let _lock = crate::clipboard::test_support::lock_clipboard();
        let (mut app, resp_tx) = new_test_app();
        let ctx = egui::Context::default();

        app.sm.handle(UiEvent::ContentReady {
            content: crate::ClipboardContent::text_only("only".into()),
            auto_copy: true,
        });
        let current_id = app.sm.current_request_id();

        resp_tx
            .send(WorkerResponse::Complete {
                result: "fresh answer".into(),
                think_content: None,
                request_id: current_id,
                incomplete: None,
                debug: crate::DebugCapture {
                    endpoint: Some("http://fresh".into()),
                    ..Default::default()
                },
            })
            .unwrap();
        app.poll_responses(&ctx);

        assert_eq!(app.req_ok, 1);
        assert_eq!(app.req_err, 0);
        assert!(app.last_debug.is_some(), "current completion's debug snapshot must surface");
    }

    #[test]
    fn settings_notice_lines() {
        let path = std::path::PathBuf::from("/x/config.toml");
        let ok = ReloadOutcome::Applied { path: path.clone(), restart_needed: vec![], models_changed: false };
        assert_eq!(settings_notice(&ok), "Saved and applied.");
        let restart = ReloadOutcome::Applied { path, restart_needed: vec!["[ui].tabs"], models_changed: false };
        assert_eq!(settings_notice(&restart), "Saved. Restart to apply [ui].tabs.");
        assert_eq!(
            settings_notice(&ReloadOutcome::Failed("invalid TOML")),
            "Saved, but reloading failed (invalid TOML) \u{2014} restart to apply."
        );
    }

    // --- format_reload_status: tray Config row after "Reload Config" ---

    #[test]
    fn reload_status_lines() {
        let path = std::path::PathBuf::from("/x/config.toml");
        let ok = ReloadOutcome::Applied { path: path.clone(), restart_needed: vec![], models_changed: false };
        assert_eq!(format_reload_status(&ok), "Config: reloaded (/x/config.toml)");
        let restart = ReloadOutcome::Applied {
            path: path.clone(),
            restart_needed: vec!["[ui].tabs", "[hotkey]"],
            models_changed: false,
        };
        assert_eq!(
            format_reload_status(&restart),
            "Config: reloaded \u{2014} restart to apply [ui].tabs, [hotkey]"
        );
        let models = ReloadOutcome::Applied { path, restart_needed: vec![], models_changed: true };
        assert_eq!(
            format_reload_status(&models),
            "Config: reloaded \u{2014} model profiles not rebuilt (restart to apply them)"
        );
        let failed = ReloadOutcome::Failed("invalid TOML");
        assert_eq!(
            format_reload_status(&failed),
            "Config: reload failed (invalid TOML) \u{2014} keeping the previous settings"
        );
    }

    // --- friendly_clipboard_error: file-list clipboard messages ---

    #[test]
    fn clipboard_error_unsupported_files_lists_names() {
        let e = crate::ClipboardError::UnsupportedFiles(vec!["a.pdf".into(), "b.docx".into()]);
        assert_eq!(
            friendly_clipboard_error(&e),
            "Unsupported file type: a.pdf, b.docx. Text and PNG files can be sent."
        );
        let many = crate::ClipboardError::UnsupportedFiles(
            (1..=5).map(|i| format!("f{i}.bin")).collect(),
        );
        assert_eq!(
            friendly_clipboard_error(&many),
            "Unsupported file type: f1.bin, f2.bin, f3.bin (+2 more). Text and PNG files can be sent."
        );
    }

    #[test]
    fn clipboard_error_file_too_large_and_unreadable() {
        let e = crate::ClipboardError::FileTooLarge {
            name: "big.log".into(),
            limit_bytes: 1024 * 1024,
        };
        assert_eq!(friendly_clipboard_error(&e), "big.log is too large to send (limit 1 MiB).");
        let e = crate::ClipboardError::FileReadFailed { name: "x.txt".into(), reason: "eperm".into() };
        assert_eq!(friendly_clipboard_error(&e), "Could not read x.txt.");
    }

    // --- format_retry_label: Processing label during an automatic retry ---

    #[test]
    fn retry_label_transient() {
        let d = std::time::Duration::from_millis(500);
        assert_eq!(format_retry_label(1, 2, d, false), "Retrying (1/2)\u{2026}");
    }

    #[test]
    fn retry_label_rate_limited_shows_wait() {
        let d = std::time::Duration::from_secs(2);
        assert_eq!(
            format_retry_label(1, 2, d, true),
            "Rate limited \u{b7} retrying in 2s (1/2)",
        );
        // Sub-second waits round up so the label never says "in 0s".
        let d = std::time::Duration::from_millis(1200);
        assert_eq!(
            format_retry_label(1, 2, d, true),
            "Rate limited \u{b7} retrying in 2s (1/2)",
        );
    }

    // --- format_completion_status: Result bottom-row summary ---

    #[test]
    fn completion_status_elapsed_only() {
        let debug = crate::DebugCapture { elapsed_ms: Some(2400), ..Default::default() };
        assert_eq!(format_completion_status(&debug).as_deref(), Some("\u{2713} 2.4s"));
    }

    #[test]
    fn completion_status_elapsed_and_tokens() {
        let debug =
            crate::DebugCapture { elapsed_ms: Some(2400), total_tokens: Some(850), ..Default::default() };
        assert_eq!(
            format_completion_status(&debug).as_deref(),
            Some("\u{2713} 2.4s \u{b7} 850 tokens"),
        );
    }

    #[test]
    fn completion_status_leads_with_model() {
        let debug = crate::DebugCapture {
            elapsed_ms: Some(2400),
            total_tokens: Some(850),
            model: Some("grok-4.3".into()),
            ..Default::default()
        };
        assert_eq!(
            format_completion_status(&debug).as_deref(),
            Some("grok-4.3 \u{b7} \u{2713} 2.4s \u{b7} 850 tokens"),
        );
    }

    #[test]
    fn completion_status_shortens_namespaced_model() {
        let debug = crate::DebugCapture {
            elapsed_ms: Some(1000),
            model: Some("MiniMaxAI/MiniMax-M2.5".into()),
            ..Default::default()
        };
        assert_eq!(
            format_completion_status(&debug).as_deref(),
            Some("MiniMax-M2.5 \u{b7} \u{2713} 1.0s"),
        );
    }

    #[test]
    fn short_model_label_strips_namespace_and_caps_length() {
        assert_eq!(short_model_label("grok-4.3"), "grok-4.3");
        assert_eq!(short_model_label("MiniMaxAI/MiniMax-M2.5"), "MiniMax-M2.5");
        assert_eq!(short_model_label("qwen/qwen3-32b"), "qwen3-32b");
        // 41 chars after the namespace -> capped to 24 incl. the ellipsis.
        let long = short_model_label("meta-llama/llama-4-scout-17b-16e-instruct");
        assert_eq!(long.chars().count(), 24);
        assert!(long.starts_with("llama-4-scout-17b-16e-i"));
        assert!(long.ends_with('\u{2026}'));
        // Empty / whitespace collapses to nothing.
        assert_eq!(short_model_label("  "), "");
    }

    #[test]
    fn completion_status_none_without_elapsed() {
        let debug = crate::DebugCapture { total_tokens: Some(850), ..Default::default() };
        assert_eq!(format_completion_status(&debug), None);
    }

    // --- resize grip: anchor + placement ---

    /// A grip drag sets the panel size and anchors the window's current
    /// top-left (the last position this adapter applied), so the reposition
    /// pass keeps re-asserting it while the size changes instead of
    /// re-centering (which would slide the grip away).
    #[test]
    fn resize_action_sets_the_size_and_anchors_the_top_left() {
        let _lock = crate::clipboard::test_support::lock_clipboard();
        let (mut app, _resp_tx) = new_test_app();
        let ctx = egui::Context::default();

        app.sm_handle(UiEvent::ContentReady {
            content: crate::ClipboardContent::text_only("only".into()),
            auto_copy: true,
        });
        app.last_sent_pos = Some(egui::pos2(100.0, 200.0));

        app.handle_overlay_action(&ctx, overlay::OverlayAction::Resize(egui::vec2(640.0, 420.0)));

        assert_eq!(app.panel_size, egui::vec2(640.0, 420.0));
        assert!(app.sm.user_repositioned());
        assert_eq!(app.resize_anchor, Some(egui::pos2(100.0, 200.0)));
        // No spawn point → no display bounds, so the anchor passes through.
        assert_eq!(
            app.calculate_target_position(egui::vec2(680.0, 460.0)),
            Some(egui::pos2(100.0, 200.0)),
        );
        // Further steps of the same gesture keep the first anchor.
        app.last_sent_pos = Some(egui::pos2(0.0, 0.0));
        app.handle_overlay_action(&ctx, overlay::OverlayAction::Resize(egui::vec2(700.0, 500.0)));
        assert_eq!(app.resize_anchor, Some(egui::pos2(100.0, 200.0)));
    }

    /// The anchor lives while the size keeps changing; the first settled
    /// frame drops it, so the next trigger centers the (still fixed-size)
    /// panel on the new spawn point instead of reusing a stale corner.
    #[test]
    fn a_settled_frame_drops_the_resize_anchor() {
        let _lock = crate::clipboard::test_support::lock_clipboard();
        let (mut app, _resp_tx) = new_test_app();
        let ctx = egui::Context::default();

        app.sm_handle(UiEvent::ContentReady {
            content: crate::ClipboardContent::text_only("only".into()),
            auto_copy: true,
        });
        app.last_sent_pos = Some(egui::pos2(100.0, 200.0));
        app.handle_overlay_action(&ctx, overlay::OverlayAction::Resize(egui::vec2(640.0, 420.0)));
        let win = app.window_size();
        app.update_viewport(&ctx, Some(win), Some(app.panel_size));
        assert!(app.resize_anchor.is_some(), "size changed this frame: still anchored");

        app.update_viewport(&ctx, Some(win), Some(app.panel_size));
        assert!(app.resize_anchor.is_none(), "unchanged size: gesture over");
        assert_eq!(app.panel_size, egui::vec2(640.0, 420.0), "the size itself stays");
    }

    /// Double-clicking the grip returns to the default size, clamped like
    /// any other size, and hands placement to the user like a drag.
    #[test]
    fn reset_size_restores_the_default_panel() {
        let _lock = crate::clipboard::test_support::lock_clipboard();
        let (mut app, _resp_tx) = new_test_app();
        let ctx = egui::Context::default();
        app.sm_handle(UiEvent::ContentReady {
            content: crate::ClipboardContent::text_only("only".into()),
            auto_copy: true,
        });
        app.handle_overlay_action(&ctx, overlay::OverlayAction::Resize(egui::vec2(100.0, 100.0)));
        assert_eq!(app.panel_size, theme::size::MIN_PANEL, "clamped");

        app.handle_overlay_action(&ctx, overlay::OverlayAction::ResetSize);
        assert_eq!(app.panel_size, theme::size::DEFAULT_PANEL);
        assert!(app.sm.user_repositioned());
    }

    // --- settings panel takes over a visible overlay ---

    /// Opening Settings while a result is showing must not hide the window:
    /// `HideWindow`'s `Visible(false)` is applied at frame end, after the
    /// native show, so the panel would open behind a hidden window.
    #[test]
    fn open_settings_over_a_result_keeps_the_window_visible() {
        let _lock = crate::clipboard::test_support::lock_clipboard();
        let (mut app, resp_tx) = new_test_app();
        let ctx = egui::Context::default();
        app.sm_handle(UiEvent::ContentReady {
            content: crate::ClipboardContent::text_only("only".into()),
            auto_copy: true,
        });
        let current_id = app.sm.current_request_id();
        resp_tx
            .send(WorkerResponse::Complete {
                result: "answer".into(),
                think_content: None,
                request_id: current_id,
                incomplete: None,
                debug: crate::DebugCapture::default(),
            })
            .unwrap();
        app.poll_responses(&ctx);
        assert!(matches!(app.sm.state(), OverlayState::Result(_)));

        let full = ctx.run(egui::RawInput::default(), |ctx| app.open_settings(ctx));

        assert!(app.settings.is_some());
        assert_eq!(*app.sm.state(), OverlayState::Hidden);
        let hid = full
            .viewport_output
            .values()
            .flat_map(|v| v.commands.iter())
            .any(|c| matches!(c, egui::ViewportCommand::Visible(false)));
        assert!(!hid, "settings must not queue Visible(false) in the frame it opens");
    }

    /// A second "Settings…" while the panel is already open re-shows the
    /// window instead of being ignored — the recovery path for a panel that
    /// ended up hidden.
    #[test]
    fn reopening_settings_is_not_a_no_op() {
        let _lock = crate::clipboard::test_support::lock_clipboard();
        let (mut app, _resp_tx) = new_test_app();
        let ctx = egui::Context::default();
        app.open_settings(&ctx);
        assert!(app.settings.is_some());
        let full = ctx.run(egui::RawInput::default(), |ctx| app.open_settings(ctx));
        assert!(app.settings.is_some());
        // A request for a repaint is the observable "do something" here: the
        // native show has no egui footprint on macOS.
        assert!(full.viewport_output.values().any(|v| v.repaint_delay.is_zero()));
    }

    // --- placement and zoom ---

    /// `Remembered` placement reopens at the stored top-left instead of
    /// centering on the cursor; the grip anchor still wins during a drag.
    #[test]
    fn remembered_placement_anchors_the_stored_position() {
        let _lock = crate::clipboard::test_support::lock_clipboard();
        let (mut app, _resp_tx) = new_test_app();
        app.spawn_position = Some(egui::pos2(900.0, 900.0));
        let win = egui::vec2(552.0, 420.0);

        app.placement = PanelPlacement::Remembered;
        app.remembered_pos = None;
        let centered = app.calculate_target_position(win);
        app.remembered_pos = Some(egui::pos2(300.0, 400.0));
        // No display bounds in tests (mock platform), so the anchor passes through.
        assert_eq!(app.calculate_target_position(win), Some(egui::pos2(300.0, 400.0)));
        assert_ne!(centered, Some(egui::pos2(300.0, 400.0)), "without a stored position it centers");

        // Inside the mock display's work area, so no clamping applies.
        app.resize_anchor = Some(egui::pos2(60.0, 80.0));
        assert_eq!(app.calculate_target_position(win), Some(egui::pos2(60.0, 80.0)));
    }

    /// A zoom change re-sends the (unchanged) point size, so eframe applies
    /// the new pixels-per-point to the window; the same size at the same zoom
    /// is not re-sent.
    #[test]
    fn zoom_change_resends_the_window_size() {
        let _lock = crate::clipboard::test_support::lock_clipboard();
        let (mut app, _resp_tx) = new_test_app();
        let ctx = egui::Context::default();
        let win = app.window_size();
        let inner_sizes = |full: &egui::FullOutput| {
            full.viewport_output
                .values()
                .flat_map(|v| v.commands.iter())
                .filter(|c| matches!(c, egui::ViewportCommand::InnerSize(_)))
                .count()
        };

        let first = ctx.run(egui::RawInput::default(), |ctx| {
            app.update_viewport(ctx, Some(win), Some(app.panel_size));
        });
        assert_eq!(inner_sizes(&first), 1);
        let same = ctx.run(egui::RawInput::default(), |ctx| {
            app.update_viewport(ctx, Some(win), Some(app.panel_size));
        });
        assert_eq!(inner_sizes(&same), 0, "unchanged size and zoom: nothing sent");

        let _ = ctx.run(egui::RawInput::default(), |ctx| ctx.set_zoom_factor(1.5));
        let zoomed = ctx.run(egui::RawInput::default(), |ctx| {
            assert_eq!(ctx.zoom_factor(), 1.5);
            app.update_viewport(ctx, Some(win), Some(app.panel_size));
        });
        assert_eq!(inner_sizes(&zoomed), 1, "zoom changed: size re-sent");
    }
}
