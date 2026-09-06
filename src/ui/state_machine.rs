//! Pure UI state machine — no egui dependency.
//!
//! Receives [`UiEvent`]s and returns [`UiEffect`]s that the adapter layer
//! (OverlayApp) must execute.  This separation makes the state transition
//! logic fully unit-testable.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::{ClipboardContent, ProcessMode, RephraseLength, RephraseParams, RephraseStyle, ThinkingMode};

/// Modes offered for an image-only clipboard (no usable text): the image-consuming
/// modes, projected from [`ProcessMode::consumes_images`] over [`ProcessMode::ALL`]
/// so this stays a view of that single predicate rather than a second source of
/// truth. Preserves `ALL` order, matching the text branch of `available_modes`.
fn image_consuming_modes() -> &'static [ProcessMode] {
    static MODES: OnceLock<Vec<ProcessMode>> = OnceLock::new();
    MODES.get_or_init(|| {
        ProcessMode::ALL
            .iter()
            .copied()
            .filter(|m| m.consumes_images())
            .collect()
    })
}

/// Modes offered for `content` — the single rule behind both the tab bar
/// ([`StateMachine::available_modes`]) and the mode fallback on new content.
fn modes_for(content: &ClipboardContent) -> &'static [ProcessMode] {
    if content.is_image_only() {
        image_consuming_modes()
    } else {
        ProcessMode::ALL
    }
}

// ---------------------------------------------------------------------------
// OverlayState
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum OverlayState {
    Hidden,
    /// Double-tap selection capture is running on a background thread; the overlay
    /// shows a spinner but has no content yet. Transitions to Processing when the
    /// captured content arrives, or Error if capture fails.
    Capturing,
    Processing,
    Result(String),
    Error(String),
}

impl OverlayState {
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::Hidden => "Hidden",
            Self::Capturing => "Capturing",
            Self::Processing => "Processing",
            Self::Result(_) => "Result",
            Self::Error(_) => "Error",
        }
    }

    fn same_variant(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

/// A revision round as recorded here: the API turn plus the think block of
/// the reply being revised, restored on undo.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RevisionRound {
    turn: crate::RevisionTurn,
    think_before: Option<String>,
}

/// A finished result for one cache key with the rounds that produced it.
#[derive(Debug, Clone)]
struct CacheEntry {
    text: String,
    think: Option<String>,
    rounds: Vec<RevisionRound>,
}

/// Where the content being processed came from. Shown as a badge in the
/// overlay so a mis-detected gesture (a slow double-tap resolving to a
/// single-tap and sending stale clipboard content) is visibly different (#50).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    /// Double-tap: selection captured via simulated copy.
    Selection,
    /// Single-tap: existing clipboard content.
    Clipboard,
}

impl CaptureSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Selection => "Selection",
            Self::Clipboard => "Clipboard",
        }
    }
}

// ---------------------------------------------------------------------------
// UiEvent / UiEffect
// ---------------------------------------------------------------------------

/// Events fed into the state machine by the adapter layer.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// Trigger pressed: show the picking overlay. For a double-tap (`source:
    /// Selection`) the selection is captured on a background thread; for a
    /// single-tap (`source: Clipboard`) the clipboard was already read and is
    /// committed on modifier release.
    CaptureStarted { source: CaptureSource },
    /// Clipboard content ready for processing.
    /// `auto_copy`: when true, auto-copy the result to clipboard (double-tap behavior).
    ContentReady { content: ClipboardContent, auto_copy: bool },
    /// Worker completed successfully.
    WorkerResult {
        text: String,
        think_content: Option<String>,
        request_id: u64,
        /// `Some(reason)` when the reply was cut short but partial content was
        /// received; shown as a banner above the result. `None` = clean result.
        incomplete: Option<String>,
    },
    /// Worker detected a think block beginning (streaming only).
    ThinkStarted { request_id: u64 },
    /// Worker scheduled an automatic retry of the in-flight request; `label`
    /// is the ready-to-show status text (e.g. "Rate limited · retrying in 2s").
    RetryScheduled { request_id: u64, label: String },
    /// Worker reported an error.
    WorkerError { message: String, request_id: u64 },
    /// User pressed close / Escape.
    UserClose,
    /// User pressed cancel during processing.
    UserCancel,
    /// User switched processing mode via tab bar.
    UserSwitchMode(ProcessMode),
    /// User changed the rephrase style parameter.
    UserChangeRephraseStyle(RephraseStyle),
    /// User changed the rephrase length parameter.
    UserChangeRephraseLength(RephraseLength),
    /// User changed the thinking mode for the current ProcessMode.
    UserChangeThinkingMode(ThinkingMode),
    /// Worker reported thinking probe result.
    ThinkingProbeResult(bool),
    /// User picked another model profile (tray submenu or overlay badge).
    UserSelectModel(usize),
    /// User started dragging the overlay.
    UserStartDrag,
    /// User resized the overlay via the grip (the size itself is presentation
    /// state owned by the adapter); like a drag, this hands placement to the
    /// user until the next trigger.
    UserResize,
    /// Window gained focus.
    FocusGained,
    /// Window lost focus (after having been focused at least once).
    FocusLost,
    /// Streaming token from the worker (incremental response).
    StreamDelta { text: String, request_id: u64 },
    /// Clipboard operation failed (read or write).
    ClipboardError(String),
    /// User clicked the copy button in the result area.
    UserCopy,
    /// User clicked the paste/replace button in the result area.
    UserPaste,
    /// User toggled the pin (keep-open) button.
    UserTogglePin,
    /// User clicked the retry button: discard the cached result for the current
    /// request and re-send it for a fresh generation.
    UserRetry,
    /// The user submitted a revision instruction for the shown result.
    UserRevise(String),
    /// Drop the last revision round and show the reply it revised.
    UserUndoRevision,
}

/// Side effects that the adapter must execute after a state transition.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEffect {
    SendProcess {
        content: ClipboardContent,
        mode: ProcessMode,
        rephrase_params: RephraseParams,
        thinking_mode: ThinkingMode,
        /// Revision rounds applied on top of the base reply (empty = base request).
        revision: Vec<crate::RevisionTurn>,
        request_id: u64,
    },
    SendCancel,
    /// Switch the worker's active model profile before any further request.
    SelectModel(usize),
    WriteClipboard(String),
    ShowWindow,
    /// Show the overlay WITHOUT taking keyboard focus (used during capture so the
    /// user's app stays key and the simulated Cmd+C/Ctrl+C targets it, not the overlay).
    ShowWindowNoActivate,
    /// Spawn the background selection-capture (copy simulation + clipboard poll).
    StartCapture,
    HideWindow,
    CaptureMousePosition,
    /// Reset egui Area stored sizing (needed on state variant change).
    ResetAreas,
    /// Simulate paste (Cmd+V / Ctrl+V) into the previously focused app.
    PasteClipboard,
    /// Signal the in-flight background selection capture to abort before it
    /// mutates the clipboard (clear + simulated Cmd+C), e.g. on cancel/close.
    CancelCapture,
}

// ---------------------------------------------------------------------------
// StateMachine
// ---------------------------------------------------------------------------

pub struct StateMachine {
    state: OverlayState,
    mode: ProcessMode,
    /// Original input content, retained for re-processing on mode switch.
    original_content: Option<ClipboardContent>,
    /// Monotonically increasing counter for request identification.
    next_request_id: u64,
    /// The request_id of the currently active request.
    current_request_id: u64,
    /// Current rephrase parameters (style + length); affects system prompt for Rephrase mode.
    rephrase_params: RephraseParams,
    /// Per-mode thinking override. Missing entry = use ProcessMode::default_thinking().
    mode_thinking: HashMap<ProcessMode, ThinkingMode>,
    /// Whether thinking control is available (from probe result).
    thinking_supported: bool,
    /// True after the user drags the overlay; suppresses auto-repositioning.
    user_repositioned: bool,
    /// True once the window has received focus after show_window.
    has_been_focused: bool,
    /// Current focus level, mirrored from FocusGained/FocusLost edges. Lets
    /// on_worker_result decide whether focus was held through completion.
    is_focused: bool,
    /// Result cache by cache_key. Valid only for the current original content.
    cache: HashMap<String, CacheEntry>,
    /// Revision rounds applied to the result of the current cache key
    /// (adopted from the cache entry on every key change).
    rounds: Vec<RevisionRound>,
    /// The revision round whose request is in flight; only ever set in Processing.
    pending_revision: Option<RevisionRound>,
    /// Message of a revision that failed, shown over the restored reply until
    /// the next request.
    revision_error: Option<String>,
    /// Instruction of the failed revision, handed back once to the adapter.
    failed_instruction: Option<String>,
    /// Accumulated visible streaming text (displayed during Processing).
    streaming_text: String,
    /// True once a think block has started during the current streaming request.
    think_started: bool,
    /// Status line for an automatic retry in progress (replaces the Processing
    /// label). Cleared as soon as the retried attempt produces output.
    retry_notice: Option<String>,
    /// Think block content for the current mode (set on WorkerResult).
    think_content: Option<String>,
    /// `Some(reason)` when the current Result is partial (stream cut short);
    /// the overlay shows it as a banner. Cleared on each new request / hide.
    result_incomplete: Option<String>,
    /// Whether the current session should auto-copy results to clipboard.
    /// Set by ContentReady (true for double-tap, false for single-tap).
    auto_copy: bool,
    /// When true, the overlay never auto-hides on focus loss (the user pinned it).
    /// Reset on every new trigger and on close.
    pinned: bool,
    /// Where the current content came from (selection capture vs clipboard
    /// read). Set on every trigger; shown as a badge in the overlay (#50).
    source: CaptureSource,
    /// Index of the active model profile (worker-side pool order).
    active_model: usize,
}

impl StateMachine {
    pub fn new(mode: ProcessMode) -> Self {
        Self {
            state: OverlayState::Hidden,
            mode,
            rephrase_params: RephraseParams::default(),
            mode_thinking: HashMap::new(),
            thinking_supported: false,
            original_content: None,
            next_request_id: 0,
            current_request_id: 0,
            user_repositioned: false,
            has_been_focused: false,
            is_focused: false,
            cache: HashMap::new(),
            rounds: Vec::new(),
            pending_revision: None,
            revision_error: None,
            failed_instruction: None,
            streaming_text: String::new(),
            think_started: false,
            retry_notice: None,
            think_content: None,
            result_incomplete: None,
            auto_copy: false,
            pinned: false,
            active_model: 0,
            source: CaptureSource::Clipboard,
        }
    }

    // -- Accessors --

    pub fn state(&self) -> &OverlayState {
        &self.state
    }

    /// Whether the overlay is pinned open (suppresses focus-loss auto-hide).
    pub fn pinned(&self) -> bool {
        self.pinned
    }

    pub fn mode(&self) -> ProcessMode {
        self.mode
    }

    pub fn rephrase_params(&self) -> RephraseParams {
        self.rephrase_params
    }

    /// Effective thinking mode for the current ProcessMode.
    pub fn effective_thinking_mode(&self) -> ThinkingMode {
        self.mode_thinking
            .get(&self.mode)
            .copied()
            .unwrap_or_else(|| self.mode.default_thinking())
    }

    pub fn thinking_supported(&self) -> bool {
        self.thinking_supported
    }

    pub fn streaming_text(&self) -> &str {
        &self.streaming_text
    }

    pub fn think_started(&self) -> bool {
        self.think_started
    }

    /// Status text for an automatic retry in progress, if any.
    pub fn retry_notice(&self) -> Option<&str> {
        self.retry_notice.as_deref()
    }

    pub fn think_content(&self) -> Option<&str> {
        self.think_content.as_deref()
    }

    /// Reason the current Result is partial (stream cut short), if any.
    pub fn result_incomplete(&self) -> Option<&str> {
        self.result_incomplete.as_deref()
    }

    pub fn user_repositioned(&self) -> bool {
        self.user_repositioned
    }

    pub fn auto_copy(&self) -> bool {
        self.auto_copy
    }

    /// File names behind the current content (file-list clipboard), else empty.
    pub fn content_files(&self) -> &[String] {
        self.original_content
            .as_ref()
            .map_or(&[][..], |c| c.files.as_slice())
    }

    /// Active model profile index.
    pub fn active_model(&self) -> usize {
        self.active_model
    }

    /// Startup selection (`[ui].default_model`): the worker pool starts on the
    /// same index, so no effect is needed.
    pub fn set_active_model(&mut self, index: usize) {
        self.active_model = index;
    }

    pub fn capture_source(&self) -> CaptureSource {
        self.source
    }

    /// Instructions of the revision rounds applied to the shown result, oldest first.
    pub fn revision_instructions(&self) -> Vec<&str> {
        self.rounds.iter().map(|r| r.turn.instruction.as_str()).collect()
    }

    /// A revision request is in flight.
    pub fn revising(&self) -> bool {
        self.pending_revision.is_some()
    }

    /// Message of the revision that just failed, shown over the restored reply.
    pub fn revision_error(&self) -> Option<&str> {
        self.revision_error.as_deref()
    }

    /// The instruction of the revision that just failed, handed back once so the
    /// adapter can put it back into the input.
    pub fn take_failed_instruction(&mut self) -> Option<String> {
        self.failed_instruction.take()
    }

    pub fn current_request_id(&self) -> u64 {
        self.current_request_id
    }

    pub fn variant_name(&self) -> &'static str {
        self.state.variant_name()
    }

    /// Modes available for the current content.
    /// - No content: no modes available (tabs disabled).
    /// - Image-only: the image-consuming modes ([`image_consuming_modes`]).
    /// - Text (with or without images): all modes.
    pub fn available_modes(&self) -> &[ProcessMode] {
        match &self.original_content {
            None => &[],
            Some(content) => modes_for(content),
        }
    }

    /// Text of the current content, if any.
    pub fn original_text(&self) -> Option<&str> {
        self.original_content.as_ref().and_then(|c| c.text.as_deref())
    }

    // -- Core event handler --

    pub fn handle(&mut self, event: UiEvent) -> Vec<UiEffect> {
        let effects = match event {
            UiEvent::CaptureStarted { source } => self.on_capture_started(source),
            UiEvent::ContentReady { content, auto_copy } => self.on_content_ready(content, auto_copy),
            UiEvent::WorkerResult { text, think_content, request_id, incomplete } => {
                self.on_worker_result(text, think_content, request_id, incomplete)
            }
            UiEvent::ThinkStarted { request_id } => self.on_think_started(request_id),
            UiEvent::RetryScheduled { request_id, label } => {
                self.on_retry_scheduled(request_id, label)
            }
            UiEvent::WorkerError {
                message,
                request_id,
            } => self.on_worker_error(message, request_id),
            UiEvent::UserClose => self.on_close(),
            UiEvent::UserCancel => self.on_cancel(),
            UiEvent::UserSwitchMode(mode) => self.on_switch_mode(mode),
            UiEvent::UserChangeRephraseStyle(style) => self.on_change_rephrase_style(style),
            UiEvent::UserChangeRephraseLength(length) => self.on_change_rephrase_length(length),
            UiEvent::UserChangeThinkingMode(mode) => self.on_change_thinking_mode(mode),
            UiEvent::UserSelectModel(index) => self.on_select_model(index),
            UiEvent::ThinkingProbeResult(supported) => {
                self.thinking_supported = supported;
                vec![]
            }
            UiEvent::UserStartDrag => {
                self.user_repositioned = true;
                vec![]
            }
            UiEvent::UserResize => {
                // The grip anchors the top-left; re-centering on the new size
                // would slide the grip out from under the cursor.
                self.user_repositioned = true;
                vec![]
            }
            UiEvent::FocusGained => {
                self.is_focused = true;
                self.has_been_focused = true;
                vec![]
            }
            UiEvent::StreamDelta { text, request_id } => {
                self.on_stream_delta(text, request_id)
            }
            UiEvent::FocusLost => {
                self.is_focused = false;
                self.on_focus_lost()
            }
            UiEvent::ClipboardError(msg) => self.on_clipboard_error(msg),
            UiEvent::UserCopy => self.on_user_copy(),
            UiEvent::UserPaste => self.on_user_paste(),
            UiEvent::UserTogglePin => {
                self.pinned = !self.pinned;
                vec![]
            }
            UiEvent::UserRetry => self.on_user_retry(),
            UiEvent::UserRevise(instruction) => self.on_user_revise(instruction),
            UiEvent::UserUndoRevision => self.on_user_undo_revision(),
        };

        self.check_invariants();
        effects
    }

    // -- Private transition handlers --

    fn on_capture_started(&mut self, source: CaptureSource) -> Vec<UiEffect> {
        let old_state = self.state.clone();
        self.source = source;

        // A double-tap always starts a fresh capture. Drop any prior content and
        // cached results so a capture *failure* (→ Error) can never leave stale
        // content that a later mode switch would re-process. The content is unknown
        // until the background capture completes, so we cannot key off it here.
        self.original_content = None;
        self.cache.clear();
        self.clear_revisions();
        self.mode_thinking.clear();
        self.rephrase_params = RephraseParams::default();
        self.streaming_text.clear();
        self.think_started = false;
        self.retry_notice = None;
        self.think_content = None;
        self.auto_copy = true; // capture is the double-tap (auto-copy) path
        self.user_repositioned = false;
        self.has_been_focused = false;
        self.pinned = false; // each new trigger starts unpinned
        self.state = OverlayState::Capturing;

        let mut effects = vec![
            UiEffect::CaptureMousePosition,
            UiEffect::ShowWindowNoActivate,
        ];
        if !old_state.same_variant(&self.state) {
            effects.push(UiEffect::ResetAreas);
        }
        effects.push(UiEffect::StartCapture);
        effects
    }

    fn on_content_ready(&mut self, content: ClipboardContent, auto_copy: bool) -> Vec<UiEffect> {
        let old_state = self.state.clone();

        // The selected mode may not fit the new content — a text-only mode with an
        // image-only clipboard. Fall back to the first mode the content actually
        // supports; a mode that already fits (e.g. Explain on an image) is kept.
        let modes = modes_for(&content);
        if !modes.contains(&self.mode)
            && let Some(&fallback) = modes.first()
        {
            self.mode = fallback;
        }

        // Preserve the result cache and the user's per-session mode/param choices
        // when the same content is re-triggered while the overlay is open, so the
        // other modes' cached results survive (switching to them stays instant)
        // and the chosen rephrase/thinking settings are kept. A genuinely new
        // input wipes all of it. (Closing the overlay still clears the cache via
        // reset_to_hidden.) The current mode is always re-processed below, so
        // re-triggering remains a way to get a fresh generation.
        let content_changed = self.original_content.as_ref() != Some(&content);
        self.original_content = Some(content.clone());
        // auto_copy encodes the trigger: double-tap (selection) vs single-tap
        // (clipboard). Covers re-trigger paths that skip CaptureStarted.
        self.source = if auto_copy {
            CaptureSource::Selection
        } else {
            CaptureSource::Clipboard
        };
        if content_changed {
            self.cache.clear();
            self.mode_thinking.clear();
            self.rephrase_params = RephraseParams::default();
        }
        // The current mode is re-processed from the base request either way;
        // other keys keep their rounds in the cache.
        self.clear_revisions();
        self.streaming_text.clear();
        self.think_started = false;
        self.retry_notice = None;
        self.think_content = None;
        self.auto_copy = auto_copy;
        self.next_request_id += 1;
        self.current_request_id = self.next_request_id;
        self.state = OverlayState::Processing;
        self.user_repositioned = false;
        self.has_been_focused = false;
        // Default pin state per trigger type, from config
        // ([ui].single_tap_pinned / double_tap_pinned; both default false =
        // auto-hide on focus loss). A single-tap result is not in the clipboard,
        // so set single_tap_pinned=true to keep it open. The user can also toggle
        // the pin button at runtime.
        let cfg = crate::config::get();
        self.pinned = if auto_copy {
            cfg.ui_double_tap_pinned()
        } else {
            cfg.ui_single_tap_pinned()
        };

        let mut effects = vec![
            UiEffect::CaptureMousePosition,
            UiEffect::SendProcess {
                content,
                mode: self.mode,
                rephrase_params: self.rephrase_params,
                thinking_mode: self.effective_thinking_mode(),
                request_id: self.current_request_id,
                revision: Vec::new(),
            },
        ];
        if !old_state.same_variant(&self.state) {
            effects.push(UiEffect::ResetAreas);
        }
        effects.push(UiEffect::ShowWindow);
        effects
    }

    fn on_stream_delta(&mut self, text: String, request_id: u64) -> Vec<UiEffect> {
        if request_id != self.current_request_id {
            return vec![];
        }
        if !matches!(self.state, OverlayState::Processing) {
            return vec![];
        }
        self.retry_notice = None;
        self.streaming_text.push_str(&text);
        vec![]
    }

    fn on_select_model(&mut self, index: usize) -> Vec<UiEffect> {
        if index == self.active_model {
            return vec![];
        }
        self.active_model = index;
        // Answers are model-specific: drop them so switching back re-asks
        // instead of serving the other model's reply from the cache.
        self.cache.clear();
        self.adopt_rounds();
        let mut effects = vec![UiEffect::SelectModel(index)];
        effects.extend(self.reprocess_or_cache());
        effects
    }

    fn on_retry_scheduled(&mut self, request_id: u64, label: String) -> Vec<UiEffect> {
        if request_id != self.current_request_id {
            return vec![];
        }
        if !matches!(self.state, OverlayState::Processing) {
            return vec![];
        }
        self.retry_notice = Some(label);
        vec![]
    }

    fn on_think_started(&mut self, request_id: u64) -> Vec<UiEffect> {
        if request_id != self.current_request_id {
            return vec![];
        }
        if !matches!(self.state, OverlayState::Processing) {
            return vec![];
        }
        self.think_started = true;
        self.retry_notice = None;
        vec![]
    }

    fn on_worker_result(
        &mut self,
        text: String,
        think_content: Option<String>,
        request_id: u64,
        incomplete: Option<String>,
    ) -> Vec<UiEffect> {
        if request_id != self.current_request_id {
            return vec![];
        }
        if !matches!(self.state, OverlayState::Processing) {
            return vec![];
        }
        self.streaming_text.clear();
        self.think_started = false;
        self.retry_notice = None;
        self.think_content = think_content.clone();
        self.result_incomplete = incomplete;
        self.revision_error = None;
        if let Some(round) = self.pending_revision.take() {
            self.rounds.push(round);
        }
        self.cache.insert(
            self.cache_key(),
            CacheEntry { text: text.clone(), think: think_content, rounds: self.rounds.clone() },
        );
        self.state = OverlayState::Result(text.clone());
        // Auto-hide counts only focus held at or after entering this visible
        // state (#61, #62): focus held through the transition arms the next
        // FocusLost to hide; a detached transition stays visible until a fresh
        // focus → unfocus cycle. on_worker_error / on_clipboard_error apply the
        // same rule — Result and Error are unified.
        self.has_been_focused = self.is_focused;
        let mut effects = Vec::new();
        if self.auto_copy {
            effects.push(UiEffect::WriteClipboard(text));
        }
        effects.push(UiEffect::ResetAreas);
        effects
    }

    fn on_worker_error(&mut self, message: String, request_id: u64) -> Vec<UiEffect> {
        if request_id != self.current_request_id {
            return vec![];
        }
        if !matches!(self.state, OverlayState::Processing) {
            return vec![];
        }
        self.think_started = false;
        self.retry_notice = None;
        // A failed revision keeps the reply it was revising on screen, with
        // the failure as a notice and the instruction handed back for a retry.
        if let Some(round) = self.pending_revision.take() {
            self.streaming_text.clear();
            self.failed_instruction = Some(round.turn.instruction.clone());
            self.revision_error = Some(message);
            self.restore_reply(round);
            self.has_been_focused = self.is_focused;
            return vec![UiEffect::ResetAreas];
        }
        self.state = OverlayState::Error(message);
        // Same auto-hide rule as on_worker_result (#62): if focus was held
        // through the failure the user has seen the error, so the next
        // FocusLost dismisses it; a detached error stays until the user
        // focuses it and leaves. Resetting to false here instead would strand
        // a focused error open (no fresh FocusGained edge ever re-arms it).
        self.has_been_focused = self.is_focused;
        vec![UiEffect::ResetAreas]
    }

    /// Resets all transient state and transitions to Hidden.
    fn reset_to_hidden(&mut self) {
        self.state = OverlayState::Hidden;
        self.original_content = None;
        self.cache.clear();
        self.clear_revisions();
        self.streaming_text.clear();
        self.think_started = false;
        self.retry_notice = None;
        self.think_content = None;
        self.result_incomplete = None;
        self.has_been_focused = false;
        self.is_focused = false;
        self.auto_copy = false;
        self.user_repositioned = false;
        self.pinned = false;
    }

    fn on_close(&mut self) -> Vec<UiEffect> {
        match self.state {
            OverlayState::Hidden => vec![],
            // Closing mid-request (Escape or the ✕ button) must also cancel the
            // in-flight LLM request, or it runs to completion and its response is
            // silently dropped.
            OverlayState::Processing => {
                self.reset_to_hidden();
                vec![UiEffect::SendCancel, UiEffect::HideWindow]
            }
            // Closing mid-capture must abort the background capture before it
            // clears the clipboard and fires Cmd+C, or it corrupts the clipboard.
            OverlayState::Capturing => {
                self.reset_to_hidden();
                vec![UiEffect::CancelCapture, UiEffect::HideWindow]
            }
            _ => {
                self.reset_to_hidden();
                vec![UiEffect::HideWindow]
            }
        }
    }

    fn on_cancel(&mut self) -> Vec<UiEffect> {
        // Cancelling a revision returns to the reply it was revising; only a
        // base request has nothing to fall back to.
        if let Some(round) = self.pending_revision.take() {
            self.streaming_text.clear();
            self.think_started = false;
            self.retry_notice = None;
            self.restore_reply(round);
            return vec![UiEffect::SendCancel, UiEffect::ResetAreas];
        }
        match self.state {
            // In-flight LLM request: cancel it and hide.
            OverlayState::Processing => {
                self.reset_to_hidden();
                vec![UiEffect::SendCancel, UiEffect::HideWindow]
            }
            // Capture in flight: there is no LLM request yet. Abort the background
            // capture (its result is also ignored via the adapter's seq + state
            // gate) so it cannot clear the clipboard and fire Cmd+C after cancel.
            OverlayState::Capturing => {
                self.reset_to_hidden();
                vec![UiEffect::CancelCapture, UiEffect::HideWindow]
            }
            _ => vec![],
        }
    }

    fn on_switch_mode(&mut self, new_mode: ProcessMode) -> Vec<UiEffect> {
        if self.mode == new_mode {
            return vec![];
        }
        // Block switch to unavailable modes when content is loaded
        // (e.g. image-only → Translate). When no content is loaded (Hidden),
        // allow free mode switching to set the default for the next trigger.
        if self.original_content.is_some() && !self.available_modes().contains(&new_mode) {
            return vec![];
        }
        self.mode = new_mode;
        // The cache key (computed inside) now reflects the new mode.
        self.adopt_rounds();
        self.reprocess_or_cache()
    }

    /// Applies a cached result: updates think_content and state,
    /// returns [WriteClipboard, ResetAreas].
    fn apply_cached_result(&mut self, text: String, think_content: Option<String>) -> Vec<UiEffect> {
        self.think_content = think_content;
        // Cached results carry no truncation state; clear any stale banner.
        self.result_incomplete = None;
        self.state = OverlayState::Result(text.clone());
        let mut effects = Vec::new();
        if self.auto_copy {
            effects.push(UiEffect::WriteClipboard(text));
        }
        effects.push(UiEffect::ResetAreas);
        effects
    }

    fn on_focus_lost(&mut self) -> Vec<UiEffect> {
        if matches!(self.state, OverlayState::Hidden) || !self.has_been_focused {
            return vec![];
        }
        // Pinned: the user asked to keep the overlay open — never auto-hide,
        // regardless of state (a Processing request keeps running).
        if self.pinned {
            return vec![];
        }
        // Processing: keep the overlay and the request alive (#17). Switching
        // to the paste target while waiting is natural, not a cancel — that
        // stays explicit (Esc / ✕ / Cancel). The result then lands in the
        // still-visible overlay; FocusLost is a transition event, so the
        // overlay closes only once the user focuses it again and leaves
        // (or via Esc / ✕ / ↩).
        if matches!(self.state, OverlayState::Processing) {
            return vec![];
        }
        // Not pinned, not processing: auto-hide.
        self.reset_to_hidden();
        vec![UiEffect::HideWindow]
    }

    fn on_user_copy(&self) -> Vec<UiEffect> {
        if let OverlayState::Result(text) = &self.state {
            vec![UiEffect::WriteClipboard(text.clone())]
        } else {
            vec![]
        }
    }

    fn on_user_paste(&mut self) -> Vec<UiEffect> {
        if let OverlayState::Result(text) = &self.state {
            let text = text.clone();
            let mut effects = vec![UiEffect::WriteClipboard(text)];
            self.reset_to_hidden();
            effects.push(UiEffect::HideWindow);
            effects.push(UiEffect::PasteClipboard);
            effects
        } else {
            vec![]
        }
    }

    /// User clicked retry: evict the cached result for the current request key
    /// and re-send, so the request produces a fresh generation instead of
    /// replaying the cache. From Error the eviction is a no-op (errors are
    /// never cached) and this simply re-sends the failed request.
    fn on_user_retry(&mut self) -> Vec<UiEffect> {
        if !matches!(self.state, OverlayState::Result(_) | OverlayState::Error(_)) {
            return vec![];
        }
        self.cache.remove(&self.cache_key());
        self.reprocess_or_cache()
    }

    /// Submit a revision: the shown reply becomes the round's `reply_before`
    /// and the request carries the whole chain (windowed by the client).
    fn on_user_revise(&mut self, instruction: String) -> Vec<UiEffect> {
        let instruction = instruction.trim().to_owned();
        let OverlayState::Result(text) = &self.state else { return vec![] };
        if instruction.is_empty() {
            return vec![];
        }
        let Some(content) = self.original_content.clone() else { return vec![] };
        self.pending_revision = Some(RevisionRound {
            turn: crate::RevisionTurn { reply_before: text.clone(), instruction },
            think_before: self.think_content.take(),
        });
        self.streaming_text.clear();
        self.think_started = false;
        self.retry_notice = None;
        self.result_incomplete = None;
        self.revision_error = None;
        self.failed_instruction = None;
        self.next_request_id += 1;
        self.current_request_id = self.next_request_id;
        self.state = OverlayState::Processing;
        vec![
            UiEffect::SendProcess {
                content,
                mode: self.mode,
                rephrase_params: self.rephrase_params,
                thinking_mode: self.effective_thinking_mode(),
                revision: self.revision_chain(),
                request_id: self.current_request_id,
            },
            UiEffect::ResetAreas,
        ]
    }

    /// Drop the last round: its `reply_before` is shown again and becomes the
    /// cached result for this key.
    fn on_user_undo_revision(&mut self) -> Vec<UiEffect> {
        if !matches!(self.state, OverlayState::Result(_)) {
            return vec![];
        }
        let Some(round) = self.rounds.pop() else { return vec![] };
        self.revision_error = None;
        self.failed_instruction = None;
        self.restore_reply(round);
        let OverlayState::Result(text) = &self.state else { unreachable!("restore_reply sets Result") };
        self.cache.insert(
            self.cache_key(),
            CacheEntry { text: text.clone(), think: self.think_content.clone(), rounds: self.rounds.clone() },
        );
        let mut effects = Vec::new();
        if self.auto_copy {
            effects.push(UiEffect::WriteClipboard(text.clone()));
        }
        effects.push(UiEffect::ResetAreas);
        effects
    }

    /// Show the reply a round revised (its think block included).
    fn restore_reply(&mut self, round: RevisionRound) {
        self.think_content = round.think_before;
        self.result_incomplete = None;
        self.state = OverlayState::Result(round.turn.reply_before);
    }

    /// The rounds the current key's cached result was produced with (none for
    /// a key without a result). Called on every cache-key change.
    fn adopt_rounds(&mut self) {
        self.rounds = self.cache.get(&self.cache_key()).map(|e| e.rounds.clone()).unwrap_or_default();
    }

    /// Applied rounds plus the one in flight, as the request carries them.
    fn revision_chain(&self) -> Vec<crate::RevisionTurn> {
        self.rounds.iter().chain(self.pending_revision.iter()).map(|r| r.turn.clone()).collect()
    }

    fn clear_revisions(&mut self) {
        self.rounds.clear();
        self.pending_revision = None;
        self.revision_error = None;
        self.failed_instruction = None;
    }

    fn on_clipboard_error(&mut self, msg: String) -> Vec<UiEffect> {
        // Must NOT emit WriteClipboard to avoid infinite recursion.
        self.state = OverlayState::Error(msg);
        // Unified auto-hide rule (#62): track focus at entry. The ShowWindow
        // below activates the overlay (capture used ShowWindowNoActivate, so
        // is_focused is false here), and the resulting FocusGained re-arms
        // has_been_focused — so the user sees the error, then a FocusLost
        // dismisses it.
        self.has_been_focused = self.is_focused;
        vec![UiEffect::ResetAreas, UiEffect::ShowWindow]
    }

    /// Cache key identifying the request that would be sent for the current state.
    ///
    /// Keyed on exactly the inputs that change the system prompt + thinking control
    /// — never on the rendered prompt string. This keeps the state machine
    /// independent of the global `config` (the prompt text) and is cheaper than
    /// formatting the full prompt. Two states that would produce an identical
    /// request share a cache entry; modes whose prompt ignores an axis omit it.
    fn cache_key(&self) -> String {
        let thinking = self.effective_thinking_mode();
        match self.mode {
            ProcessMode::Translate => format!("translate|{thinking:?}"),
            ProcessMode::Summarize => format!("summarize|{thinking:?}"),
            ProcessMode::Rephrase => format!(
                "rephrase|{:?}|{:?}|{thinking:?}",
                self.rephrase_params.style, self.rephrase_params.length,
            ),
            ProcessMode::Explain => format!("explain|{thinking:?}"),
            ProcessMode::Transcribe => format!("transcribe|{thinking:?}"),
        }
    }

    fn on_change_rephrase_style(&mut self, style: RephraseStyle) -> Vec<UiEffect> {
        if self.rephrase_params.style == style {
            return vec![];
        }
        self.rephrase_params.style = style;
        self.on_rephrase_params_changed()
    }

    fn on_change_rephrase_length(&mut self, length: RephraseLength) -> Vec<UiEffect> {
        if self.rephrase_params.length == length {
            return vec![];
        }
        self.rephrase_params.length = length;
        self.on_rephrase_params_changed()
    }

    fn on_change_thinking_mode(&mut self, thinking: ThinkingMode) -> Vec<UiEffect> {
        if self.effective_thinking_mode() == thinking {
            return vec![];
        }
        self.mode_thinking.insert(self.mode, thinking);
        self.adopt_rounds();
        self.reprocess_or_cache()
    }

    /// Re-process or serve from cache when rephrase params change (Rephrase mode only).
    fn on_rephrase_params_changed(&mut self) -> Vec<UiEffect> {
        if self.mode != ProcessMode::Rephrase {
            return vec![];
        }
        self.adopt_rounds();
        self.reprocess_or_cache()
    }

    /// Re-process the current content, or serve it from cache, for the current
    /// (mode, rephrase params, thinking) combination. Shared by mode switches and
    /// rephrase/thinking parameter changes: the caller mutates the relevant field
    /// first, then this computes the cache key and dispatches uniformly.
    ///
    /// - Processing: cache hit cancels the in-flight request and shows the cached
    ///   result; cache miss cancels and re-sends with a new request id.
    /// - Result/Error: cache hit shows it directly; cache miss re-sends and
    ///   returns to Processing.
    /// - Hidden/Capturing: nothing to re-process.
    fn reprocess_or_cache(&mut self) -> Vec<UiEffect> {
        let key = self.cache_key();
        match &self.state {
            OverlayState::Processing => {
                // Whatever was in flight is cancelled below, a revision included.
                self.pending_revision = None;
                if let Some(CacheEntry { text, think, .. }) = self.cache.get(&key).cloned() {
                    self.streaming_text.clear();
                    self.think_started = false;
                    self.retry_notice = None;
                    let mut effects = self.apply_cached_result(text, think);
                    effects.insert(0, UiEffect::SendCancel);
                    effects
                } else if let Some(content) = self.original_content.clone() {
                    self.streaming_text.clear();
                    self.think_started = false;
                    self.retry_notice = None;
                    self.think_content = None;
                    self.next_request_id += 1;
                    self.current_request_id = self.next_request_id;
                    vec![
                        UiEffect::SendCancel,
                        UiEffect::SendProcess {
                            content,
                            mode: self.mode,
                            rephrase_params: self.rephrase_params,
                            thinking_mode: self.effective_thinking_mode(),
                            request_id: self.current_request_id,
                            revision: self.revision_chain(),
                        },
                    ]
                } else {
                    vec![UiEffect::SendCancel]
                }
            }
            OverlayState::Result(_) | OverlayState::Error(_) => {
                if let Some(CacheEntry { text, think, .. }) = self.cache.get(&key).cloned() {
                    self.apply_cached_result(text, think)
                } else if let Some(content) = self.original_content.clone() {
                    // Clear any partial stream left over from a request that
                    // errored mid-stream, so the new Processing view starts clean.
                    self.streaming_text.clear();
                    self.think_started = false;
                    self.retry_notice = None;
                    self.think_content = None;
                    self.next_request_id += 1;
                    self.current_request_id = self.next_request_id;
                    self.state = OverlayState::Processing;
                    vec![
                        UiEffect::SendProcess {
                            content,
                            mode: self.mode,
                            rephrase_params: self.rephrase_params,
                            thinking_mode: self.effective_thinking_mode(),
                            request_id: self.current_request_id,
                            revision: self.revision_chain(),
                        },
                        UiEffect::ResetAreas,
                    ]
                } else {
                    vec![]
                }
            }
            // No content loaded (idle) or capture still running: nothing to re-process.
            OverlayState::Hidden | OverlayState::Capturing => vec![],
        }
    }

    fn check_invariants(&self) {
        debug_assert!(
            !matches!(self.state, OverlayState::Processing) || self.original_content.is_some(),
            "invariant violated: Processing state requires original_content"
        );
        debug_assert!(
            self.pending_revision.is_none() || matches!(self.state, OverlayState::Processing),
            "invariant violated: a pending revision requires Processing"
        );
        debug_assert!(
            !matches!(self.state, OverlayState::Hidden) || self.original_content.is_none(),
            "invariant violated: Hidden state should have no original_content"
        );
        debug_assert!(
            !matches!(self.state, OverlayState::Capturing) || self.original_content.is_none(),
            "invariant violated: Capturing state should have no original_content (content not yet captured)"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn new_sm() -> StateMachine {
        StateMachine::new(ProcessMode::Translate)
    }

    /// Helper: feed ContentReady with text-only content and return the effects.
    /// Uses `auto_copy: true` (double-tap) to preserve existing test behavior.
    fn start_processing(sm: &mut StateMachine, text: &str) -> Vec<UiEffect> {
        sm.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only(text.to_string()),
            auto_copy: true,
        })
    }

    /// Helper: get the request_id from the last SendProcess effect.
    fn last_request_id(effects: &[UiEffect]) -> u64 {
        effects
            .iter()
            .rev()
            .find_map(|e| match e {
                UiEffect::SendProcess { request_id, .. } => Some(*request_id),
                _ => None,
            })
            .expect("no SendProcess effect found")
    }

    // === Basic state transitions ===

    #[test]
    fn hidden_to_processing_on_text_ready() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");

        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(effects.contains(&UiEffect::CaptureMousePosition));
        assert!(effects.contains(&UiEffect::ShowWindow));
        assert!(effects.contains(&UiEffect::ResetAreas));
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    #[test]
    fn processing_to_result_on_worker_result() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);

        let effects = sm.handle(UiEvent::WorkerResult {
            text: "translated".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        assert_eq!(*sm.state(), OverlayState::Result("translated".into()));
        assert!(effects.contains(&UiEffect::WriteClipboard("translated".into())));
        assert!(!effects.contains(&UiEffect::ShowWindow));
        assert!(effects.contains(&UiEffect::ResetAreas));
    }

    #[test]
    fn processing_to_error_on_worker_error() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);

        let effects = sm.handle(UiEvent::WorkerError {
            message: "fail".into(),
            request_id: rid,
        });

        assert_eq!(*sm.state(), OverlayState::Error("fail".into()));
        assert!(!effects.contains(&UiEffect::ShowWindow));
        assert!(effects.contains(&UiEffect::ResetAreas));
        // Must NOT contain WriteClipboard.
        assert!(!effects.iter().any(|e| matches!(e, UiEffect::WriteClipboard(_))));
    }

    #[test]
    fn result_to_hidden_on_close() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "ok".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        let effects = sm.handle(UiEvent::UserClose);

        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn error_to_hidden_on_close() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerError {
            message: "err".into(),
            request_id: rid,
        });

        let effects = sm.handle(UiEvent::UserClose);

        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn processing_to_hidden_on_cancel() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");

        let effects = sm.handle(UiEvent::UserCancel);

        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::SendCancel));
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn close_during_processing_cancels_request() {
        // Closing via the ✕ button (or Escape) while a request is in flight must
        // cancel it, not just hide — otherwise the request leaks and its response
        // is silently dropped.
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");

        let effects = sm.handle(UiEvent::UserClose);

        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::SendCancel));
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn cancel_during_capturing_aborts_capture() {
        let mut sm = new_sm();
        sm.handle(UiEvent::CaptureStarted { source: CaptureSource::Selection });
        assert_eq!(*sm.state(), OverlayState::Capturing);

        let effects = sm.handle(UiEvent::UserCancel);

        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::CancelCapture));
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn close_during_capturing_aborts_capture() {
        let mut sm = new_sm();
        sm.handle(UiEvent::CaptureStarted { source: CaptureSource::Selection });
        assert_eq!(*sm.state(), OverlayState::Capturing);

        let effects = sm.handle(UiEvent::UserClose);

        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::CancelCapture));
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    // === Mode switch ===

    #[test]
    fn switch_mode_during_processing_cancels_and_resends() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let old_rid = last_request_id(&effects);

        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));

        assert_eq!(*sm.state(), OverlayState::Processing);
        assert_eq!(sm.mode(), ProcessMode::Rephrase);
        assert!(effects.contains(&UiEffect::SendCancel));

        let new_rid = last_request_id(&effects);
        assert_ne!(old_rid, new_rid, "request_id should increment");
    }

    #[test]
    fn switch_mode_from_result_reprocesses() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "ok".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));

        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { mode: ProcessMode::Rephrase, .. })));
        assert!(effects.contains(&UiEffect::ResetAreas));
        // Should NOT contain SendCancel (no in-flight request from Result state).
        assert!(!effects.contains(&UiEffect::SendCancel));
    }

    #[test]
    fn switch_mode_same_mode_ignored() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");

        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Translate));

        assert!(effects.is_empty());
    }

    #[test]
    fn switch_mode_from_hidden_changes_mode_only() {
        let mut sm = new_sm();

        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));

        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert_eq!(sm.mode(), ProcessMode::Rephrase);
        assert!(effects.is_empty());
    }

    // === Request ID / stale response rejection ===

    #[test]
    fn stale_result_ignored() {
        let mut sm = new_sm();
        let effects1 = start_processing(&mut sm, "first");
        let rid1 = last_request_id(&effects1);

        // Start a second request (simulating mode switch or new trigger).
        let effects2 = start_processing(&mut sm, "second");
        let rid2 = last_request_id(&effects2);
        assert_ne!(rid1, rid2);

        // Stale response from first request arrives.
        let effects = sm.handle(UiEvent::WorkerResult {
            text: "stale".into(),
            think_content: None,
            request_id: rid1, incomplete: None });

        assert!(effects.is_empty());
        assert_eq!(*sm.state(), OverlayState::Processing);

        // Current response works.
        let effects = sm.handle(UiEvent::WorkerResult {
            text: "current".into(),
            think_content: None,
            request_id: rid2, incomplete: None });

        assert_eq!(*sm.state(), OverlayState::Result("current".into()));
        assert!(!effects.is_empty());
    }

    #[test]
    fn stale_error_ignored() {
        let mut sm = new_sm();
        let effects1 = start_processing(&mut sm, "first");
        let rid1 = last_request_id(&effects1);

        start_processing(&mut sm, "second");

        let effects = sm.handle(UiEvent::WorkerError {
            message: "stale error".into(),
            request_id: rid1,
        });

        assert!(effects.is_empty());
        assert_eq!(*sm.state(), OverlayState::Processing);
    }

    #[test]
    fn request_id_increments_on_each_process() {
        let mut sm = new_sm();
        let e1 = start_processing(&mut sm, "a");
        let r1 = last_request_id(&e1);

        let e2 = start_processing(&mut sm, "b");
        let r2 = last_request_id(&e2);

        let e3 = start_processing(&mut sm, "c");
        let r3 = last_request_id(&e3);

        assert!(r1 < r2 && r2 < r3);
    }

    // === original_text lifecycle ===

    #[test]
    fn original_text_set_on_text_ready() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");

        assert_eq!(sm.original_text(), Some("hello"));
    }

    #[test]
    fn original_text_cleared_on_close() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "ok".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        sm.handle(UiEvent::UserClose);

        assert_eq!(sm.original_text(), None);
    }

    #[test]
    fn original_text_cleared_on_cancel() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");

        sm.handle(UiEvent::UserCancel);

        assert_eq!(sm.original_text(), None);
    }

    #[test]
    fn original_text_retained_during_mode_switch() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");

        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));

        assert_eq!(sm.original_text(), Some("hello"));
    }

    // === Focus loss ===

    #[test]
    fn focus_lost_hides_when_focused() {
        let mut sm = new_sm();
        // Processing no longer auto-hides (#17), so exercise the generic
        // focus-loss path from the Result state.
        start_processing(&mut sm, "hello");
        let rid = sm.current_request_id();
        sm.handle(UiEvent::WorkerResult {
            text: "done".into(),
            think_content: None,
            request_id: rid, incomplete: None });
        sm.handle(UiEvent::FocusGained);

        let effects = sm.handle(UiEvent::FocusLost);

        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn focus_lost_ignored_before_focus() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");
        // Don't call set_focused().

        let effects = sm.handle(UiEvent::FocusLost);

        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(effects.is_empty());
    }

    #[test]
    fn focus_lost_ignored_when_hidden() {
        let mut sm = new_sm();
        sm.handle(UiEvent::FocusGained);

        let effects = sm.handle(UiEvent::FocusLost);

        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.is_empty());
    }

    #[test]
    fn focus_lost_during_processing_keeps_overlay_and_request() {
        // #17: switching to the paste target while waiting is natural, not a
        // cancel — the overlay and the in-flight request must stay alive.
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");
        sm.handle(UiEvent::FocusGained);

        let effects = sm.handle(UiEvent::FocusLost);

        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(effects.is_empty());
    }

    #[test]
    fn result_after_detached_processing_hides_on_refocus_cycle() {
        // #17 follow-through: the result lands in the still-visible overlay;
        // a later focus-then-unfocus cycle dismisses it.
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");
        sm.handle(UiEvent::FocusGained);
        sm.handle(UiEvent::FocusLost); // detach: user went to the paste target

        let rid = sm.current_request_id();
        sm.handle(UiEvent::WorkerResult {
            text: "done".into(),
            think_content: None,
            request_id: rid, incomplete: None });
        assert!(matches!(sm.state(), OverlayState::Result(_)));

        // User glances at the result, then leaves → overlay closes.
        sm.handle(UiEvent::FocusGained);
        let effects = sm.handle(UiEvent::FocusLost);
        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn result_after_detached_processing_survives_refired_focus_lost() {
        // #61: a result landing while the user is in another app must stay
        // visible — even if the adapter (wrongly) re-delivers FocusLost right
        // after the Processing → Result transition without a new focus cycle.
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");
        sm.handle(UiEvent::FocusGained);
        sm.handle(UiEvent::FocusLost); // detach: user went to the paste target

        let rid = sm.current_request_id();
        sm.handle(UiEvent::WorkerResult {
            text: "done".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        let effects = sm.handle(UiEvent::FocusLost);
        assert!(matches!(sm.state(), OverlayState::Result(_)));
        assert!(effects.is_empty());
    }

    #[test]
    fn result_with_focus_held_through_completion_hides_on_focus_loss() {
        // #61 spec: focus held at the moment of completion counts as focused-
        // in-Result, so leaving afterwards hides without a fresh focus cycle.
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");
        sm.handle(UiEvent::FocusGained);

        let rid = sm.current_request_id();
        sm.handle(UiEvent::WorkerResult {
            text: "done".into(),
            think_content: None,
            request_id: rid, incomplete: None });
        assert!(matches!(sm.state(), OverlayState::Result(_)));

        let effects = sm.handle(UiEvent::FocusLost);
        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn error_with_focus_held_through_failure_hides_on_focus_loss() {
        // #62: Error is unified with Result — focus held through the failure
        // means the user has seen the error, so the next FocusLost dismisses
        // it (no fresh focus cycle required).
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");
        sm.handle(UiEvent::FocusGained);

        let rid = sm.current_request_id();
        sm.handle(UiEvent::WorkerError {
            message: "boom".into(),
            request_id: rid,
        });
        assert!(matches!(sm.state(), OverlayState::Error(_)));

        let effects = sm.handle(UiEvent::FocusLost);
        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn focus_lost_hides_unpinned_single_tap_result() {
        let mut sm = new_sm();
        // Single-tap: with the default config ([ui].single_tap_pinned = false) the
        // result starts unpinned, so focus loss auto-hides it like a double-tap.
        let effects = sm.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only("hello".into()),
            auto_copy: false,
        });
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult { text: "translated".into(), think_content: None, request_id: rid, incomplete: None });
        sm.handle(UiEvent::FocusGained);
        assert!(!sm.pinned());

        let effects = sm.handle(UiEvent::FocusLost);
        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn focus_lost_hides_double_tap_result() {
        let mut sm = new_sm();
        // Double-tap: result is already in the clipboard, so auto-hide is safe.
        let effects = sm.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only("hello".into()),
            auto_copy: true,
        });
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult { text: "translated".into(), think_content: None, request_id: rid, incomplete: None });
        sm.handle(UiEvent::FocusGained);

        let effects = sm.handle(UiEvent::FocusLost);
        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    // === Edge cases ===

    #[test]
    fn cancel_when_not_processing_ignored() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "ok".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        // Now in Result state — cancel should do nothing.
        let effects = sm.handle(UiEvent::UserCancel);

        assert!(effects.is_empty());
        assert_eq!(*sm.state(), OverlayState::Result("ok".into()));
    }

    #[test]
    fn clipboard_error_transitions_to_error() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");

        let effects = sm.handle(UiEvent::ClipboardError("write failed".into()));

        assert_eq!(*sm.state(), OverlayState::Error("write failed".into()));
        assert!(effects.contains(&UiEffect::ShowWindow));
        assert!(effects.contains(&UiEffect::ResetAreas));
        // Must NOT contain WriteClipboard (avoid recursion).
        assert!(!effects.iter().any(|e| matches!(e, UiEffect::WriteClipboard(_))));
    }

    #[test]
    fn full_lifecycle_invariants_hold() {
        let mut sm = new_sm();

        // Hidden -> Processing
        let effects = start_processing(&mut sm, "test");
        let rid = last_request_id(&effects);
        assert!(sm.original_text().is_some());

        // Processing -> Result
        sm.handle(UiEvent::WorkerResult {
            text: "result".into(),
            think_content: None,
            request_id: rid, incomplete: None });
        assert!(sm.original_text().is_some());

        // Result -> Processing (mode switch)
        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        let rid2 = sm.current_request_id();
        assert!(sm.original_text().is_some());

        // Processing -> Result
        sm.handle(UiEvent::WorkerResult {
            text: "corrected".into(),
            think_content: None,
            request_id: rid2, incomplete: None });

        // Result -> Hidden
        sm.handle(UiEvent::UserClose);
        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(sm.original_text().is_none());
    }

    // === Summarize mode ===

    #[test]
    fn text_ready_with_summarize_mode() {
        let mut sm = StateMachine::new(ProcessMode::Summarize);
        let effects = start_processing(&mut sm, "long text to summarize");

        assert_eq!(*sm.state(), OverlayState::Processing);
        assert_eq!(sm.mode(), ProcessMode::Summarize);
        assert!(effects.iter().any(|e| matches!(
            e,
            UiEffect::SendProcess { mode: ProcessMode::Summarize, .. }
        )));
    }

    #[test]
    fn switch_to_summarize_from_result() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);

        sm.handle(UiEvent::WorkerResult {
            text: "translated".into(),
            think_content: None,
            request_id: rid, incomplete: None });
        assert_eq!(*sm.state(), OverlayState::Result("translated".into()));

        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Summarize));
        assert_eq!(*sm.state(), OverlayState::Processing);
        assert_eq!(sm.mode(), ProcessMode::Summarize);
        assert!(effects.iter().any(|e| matches!(
            e,
            UiEffect::SendProcess { mode: ProcessMode::Summarize, .. }
        )));
        assert!(effects.contains(&UiEffect::ResetAreas));
    }

    // === Explain mode ===

    #[test]
    fn text_ready_with_explain_mode() {
        let mut sm = StateMachine::new(ProcessMode::Explain);
        let effects = start_processing(&mut sm, "code to explain");

        assert_eq!(*sm.state(), OverlayState::Processing);
        assert_eq!(sm.mode(), ProcessMode::Explain);
        assert!(effects.iter().any(|e| matches!(
            e,
            UiEffect::SendProcess { mode: ProcessMode::Explain, .. }
        )));
    }

    #[test]
    fn explain_does_not_share_cache_with_other_modes() {
        let mut sm = new_sm();
        // Translate → Result
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "translated".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        // Switch to Explain: no cache hit — a fresh request goes out.
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Explain));
        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(effects.iter().any(|e| matches!(
            e,
            UiEffect::SendProcess { mode: ProcessMode::Explain, .. }
        )));
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "explained".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        // Switching back to Translate serves the translate cache, not explain's.
        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Translate));
        assert_eq!(*sm.state(), OverlayState::Result("translated".into()));
        // And returning to Explain serves the explain cache without a request.
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Explain));
        assert_eq!(*sm.state(), OverlayState::Result("explained".into()));
        assert!(!effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    // === Mode cache ===

    #[test]
    fn switch_back_to_cached_mode_from_result() {
        let mut sm = new_sm();
        // Translate → Result
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "translated".into(),
            think_content: None,
            request_id: rid, incomplete: None });
        // Switch to Correct → Result
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "corrected".into(),
            think_content: None,
            request_id: rid, incomplete: None });
        assert_eq!(*sm.state(), OverlayState::Result("corrected".into()));

        // Switch back to Translate: cache hit
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Translate));
        assert_eq!(*sm.state(), OverlayState::Result("translated".into()));
        assert!(effects.contains(&UiEffect::WriteClipboard("translated".into())));
        assert!(effects.contains(&UiEffect::ResetAreas));
        // No SendProcess — served from cache.
        assert!(!effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    #[test]
    fn switch_to_cached_mode_from_processing() {
        let mut sm = new_sm();
        // Translate → Result
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "translated".into(),
            think_content: None,
            request_id: rid, incomplete: None });
        // Switch to Correct → Processing (in-flight)
        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        assert_eq!(*sm.state(), OverlayState::Processing);

        // Switch back to Translate while Correct is still processing: cache hit
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Translate));
        assert_eq!(*sm.state(), OverlayState::Result("translated".into()));
        assert!(effects.contains(&UiEffect::SendCancel));
        assert!(effects.contains(&UiEffect::WriteClipboard("translated".into())));
        assert!(!effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    #[test]
    fn switch_to_cached_mode_from_error() {
        let mut sm = new_sm();
        // Translate → Result
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "ok".into(),
            think_content: None,
            request_id: rid, incomplete: None });
        // Switch to Correct → Error
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerError {
            message: "fail".into(),
            request_id: rid,
        });
        assert_eq!(*sm.state(), OverlayState::Error("fail".into()));

        // Switch back to Translate: cache hit
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Translate));
        assert_eq!(*sm.state(), OverlayState::Result("ok".into()));
        assert!(effects.contains(&UiEffect::WriteClipboard("ok".into())));
        assert!(!effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    #[test]
    fn new_text_ready_clears_cache() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "translated".into(),
            think_content: None,
            request_id: rid, incomplete: None });
        // Cache now has Translate → "translated"

        // New content arrives → cache cleared, re-processes
        let effects = sm.handle(UiEvent::ContentReady { content: ClipboardContent::text_only("world".into()), auto_copy: true });
        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    #[test]
    fn identical_content_ready_preserves_other_mode_cache() {
        let mut sm = new_sm();
        // Translate → cache "translated".
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult { text: "translated".into(), think_content: None, request_id: rid, incomplete: None });
        // Rephrase → cache "rephrased".
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult { text: "rephrased".into(), think_content: None, request_id: rid, incomplete: None });
        // Back to Translate (cache hit).
        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Translate));
        assert_eq!(*sm.state(), OverlayState::Result("translated".into()));

        // Re-trigger with the SAME content: the current mode still re-processes,
        // but the Rephrase cache entry must survive.
        let effects = sm.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only("hello".into()),
            auto_copy: true,
        });
        assert_eq!(*sm.state(), OverlayState::Processing);
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult { text: "translated2".into(), think_content: None, request_id: rid, incomplete: None });

        // Switching to Rephrase is served from the preserved cache — no SendProcess.
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        assert_eq!(*sm.state(), OverlayState::Result("rephrased".into()));
        assert!(!effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    #[test]
    fn identical_content_ready_preserves_rephrase_params() {
        let mut sm = new_sm();
        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        start_processing(&mut sm, "hello");
        sm.handle(UiEvent::UserChangeRephraseLength(RephraseLength::Terse));
        assert_eq!(sm.rephrase_params().length, RephraseLength::Terse);

        // Same content → params preserved.
        start_processing(&mut sm, "hello");
        assert_eq!(sm.rephrase_params().length, RephraseLength::Terse);
    }

    #[test]
    fn close_clears_cache() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "translated".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        // Close overlay → cache cleared
        sm.handle(UiEvent::UserClose);
        assert_eq!(*sm.state(), OverlayState::Hidden);

        // Re-enter with same text: should go to Processing (not cached)
        let effects = sm.handle(UiEvent::ContentReady { content: ClipboardContent::text_only("hello".into()), auto_copy: true });
        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    // === Retry ===

    #[test]
    fn retry_from_result_evicts_cache_and_resends() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "v1".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        // Retry: the cached "v1" must be evicted, so a new request is sent
        // (a cache hit would have replayed "v1" without SendProcess).
        let effects = sm.handle(UiEvent::UserRetry);
        assert_eq!(*sm.state(), OverlayState::Processing);
        let new_rid = last_request_id(&effects);
        assert!(new_rid > rid, "retry must use a fresh request id");

        // The fresh generation replaces the old result and is re-cached.
        sm.handle(UiEvent::WorkerResult {
            text: "v2".into(),
            think_content: None,
            request_id: new_rid, incomplete: None });
        assert_eq!(*sm.state(), OverlayState::Result("v2".into()));
    }

    #[test]
    fn retry_from_error_resends() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerError {
            message: "fail".into(),
            request_id: rid,
        });

        let effects = sm.handle(UiEvent::UserRetry);
        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(last_request_id(&effects) > rid);
    }

    #[test]
    fn retry_clears_stale_streaming_text_after_mid_stream_error() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::StreamDelta { text: "partial".into(), request_id: rid });
        sm.handle(UiEvent::WorkerError {
            message: "stalled".into(),
            request_id: rid,
        });

        sm.handle(UiEvent::UserRetry);
        assert_eq!(*sm.state(), OverlayState::Processing);
        assert_eq!(sm.streaming_text(), "", "stale partial stream must not survive a retry");
    }

    #[test]
    fn retry_ignored_outside_result_and_error() {
        let mut sm = new_sm();
        // Hidden: nothing to retry.
        assert!(sm.handle(UiEvent::UserRetry).is_empty());
        assert_eq!(*sm.state(), OverlayState::Hidden);

        // Processing: the in-flight request already covers it.
        start_processing(&mut sm, "hello");
        assert!(sm.handle(UiEvent::UserRetry).is_empty());
        assert_eq!(*sm.state(), OverlayState::Processing);
    }

    #[test]
    fn retry_only_evicts_current_key() {
        let mut sm = new_sm();
        // Translate result cached.
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "translated".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        // Summarize result cached.
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Summarize));
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "summary".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        // Retry Summarize, complete it, then switch back to Translate:
        // the Translate cache entry must still be served without a request.
        let effects = sm.handle(UiEvent::UserRetry);
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "summary v2".into(),
            think_content: None,
            request_id: rid, incomplete: None });
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Translate));
        assert_eq!(*sm.state(), OverlayState::Result("translated".into()));
        assert!(!effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    // === Streaming text ===

    // --- UserSelectModel: switching the model profile ---

    #[test]
    fn select_model_when_hidden_only_switches() {
        let mut sm = new_sm();
        assert_eq!(sm.active_model(), 0);
        let effects = sm.handle(UiEvent::UserSelectModel(2));
        assert_eq!(effects, vec![UiEffect::SelectModel(2)]);
        assert_eq!(sm.active_model(), 2);
        assert!(matches!(sm.state(), OverlayState::Hidden));
        assert!(sm.handle(UiEvent::UserSelectModel(2)).is_empty(), "same model is a no-op");
    }

    #[test]
    fn select_model_in_result_reprocesses_with_fresh_cache() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "old".into(),
            think_content: None,
            request_id: rid,
            incomplete: None,
        });

        let effects = sm.handle(UiEvent::UserSelectModel(1));
        let select = effects.iter().position(|e| matches!(e, UiEffect::SelectModel(1)));
        let send = effects.iter().position(|e| matches!(e, UiEffect::SendProcess { .. }));
        assert!(select.is_some() && send.is_some(), "{effects:?}");
        assert!(select < send, "worker must switch before it receives the request");
        assert!(matches!(sm.state(), OverlayState::Processing));
        assert_ne!(last_request_id(&effects), rid);

        // Switching back must not serve model 1's (or the old) cached answer.
        let effects = sm.handle(UiEvent::UserSelectModel(0));
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
        assert!(matches!(sm.state(), OverlayState::Processing));
    }

    #[test]
    fn select_model_while_processing_cancels_and_resends() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        let effects = sm.handle(UiEvent::UserSelectModel(1));
        assert!(effects.contains(&UiEffect::SelectModel(1)));
        assert!(effects.contains(&UiEffect::SendCancel));
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
        assert_ne!(last_request_id(&effects), rid);
    }

    #[test]
    fn select_model_survives_hide() {
        let mut sm = new_sm();
        sm.handle(UiEvent::UserSelectModel(1));
        start_processing(&mut sm, "hello");
        sm.handle(UiEvent::UserClose);
        assert_eq!(sm.active_model(), 1, "the model choice is a session setting, not per-trigger");
    }

    // --- RetryScheduled: automatic-retry status in Processing ---

    #[test]
    fn retry_scheduled_sets_notice_while_processing() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        assert_eq!(sm.retry_notice(), None);

        sm.handle(UiEvent::RetryScheduled { request_id: rid, label: "Retrying (1/2)".into() });
        assert_eq!(sm.retry_notice(), Some("Retrying (1/2)"));
        assert!(matches!(sm.state(), OverlayState::Processing));
    }

    #[test]
    fn retry_scheduled_stale_or_idle_ignored() {
        let mut sm = new_sm();
        sm.handle(UiEvent::RetryScheduled { request_id: 0, label: "x".into() });
        assert_eq!(sm.retry_notice(), None, "Hidden: nothing to annotate");

        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::RetryScheduled { request_id: rid + 100, label: "x".into() });
        assert_eq!(sm.retry_notice(), None, "stale request_id must not leak in");
    }

    #[test]
    fn retry_notice_cleared_once_output_arrives() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::RetryScheduled { request_id: rid, label: "Retrying".into() });

        sm.handle(UiEvent::StreamDelta { text: "foo".into(), request_id: rid });
        assert_eq!(sm.retry_notice(), None, "first delta of the retried attempt clears it");

        sm.handle(UiEvent::RetryScheduled { request_id: rid, label: "Retrying".into() });
        sm.handle(UiEvent::ThinkStarted { request_id: rid });
        assert_eq!(sm.retry_notice(), None, "think start counts as output too");
    }

    #[test]
    fn retry_notice_cleared_on_result_error_and_new_request() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::RetryScheduled { request_id: rid, label: "Retrying".into() });
        sm.handle(UiEvent::WorkerError { message: "boom".into(), request_id: rid });
        assert_eq!(sm.retry_notice(), None);

        // Retry from Error starts a new request: the notice must not carry over.
        let effects = sm.handle(UiEvent::UserRetry);
        let rid2 = last_request_id(&effects);
        assert_ne!(rid, rid2);
        assert_eq!(sm.retry_notice(), None);
        sm.handle(UiEvent::RetryScheduled { request_id: rid2, label: "Retrying".into() });
        sm.handle(UiEvent::WorkerResult {
            text: "done".into(),
            think_content: None,
            request_id: rid2,
            incomplete: None,
        });
        assert_eq!(sm.retry_notice(), None);

        // Hide clears it as well.
        let effects = start_processing(&mut sm, "again");
        let rid3 = last_request_id(&effects);
        sm.handle(UiEvent::RetryScheduled { request_id: rid3, label: "Retrying".into() });
        sm.handle(UiEvent::UserClose);
        assert_eq!(sm.retry_notice(), None);
    }

    #[test]
    fn stream_delta_appends_text() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);

        sm.handle(UiEvent::StreamDelta { text: "foo".into(), request_id: rid });
        sm.handle(UiEvent::StreamDelta { text: " bar".into(), request_id: rid });

        assert_eq!(sm.streaming_text(), "foo bar");
    }

    #[test]
    fn stream_delta_stale_ignored() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);

        // Stale request_id.
        sm.handle(UiEvent::StreamDelta { text: "stale".into(), request_id: rid + 100 });

        assert_eq!(sm.streaming_text(), "");
    }

    #[test]
    fn stream_delta_not_processing_ignored() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);

        // Transition to Result.
        sm.handle(UiEvent::WorkerResult { text: "done".into(), think_content: None, request_id: rid, incomplete: None });

        // Delta arrives after Result — ignored.
        sm.handle(UiEvent::StreamDelta { text: "late".into(), request_id: rid });

        assert_eq!(sm.streaming_text(), "");
    }

    #[test]
    fn streaming_text_cleared_on_result() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);

        sm.handle(UiEvent::StreamDelta { text: "partial".into(), request_id: rid });
        assert_eq!(sm.streaming_text(), "partial");

        sm.handle(UiEvent::WorkerResult { text: "done".into(), think_content: None, request_id: rid, incomplete: None });
        assert_eq!(sm.streaming_text(), "");
    }

    #[test]
    fn streaming_text_cleared_on_cancel() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);

        sm.handle(UiEvent::StreamDelta { text: "partial".into(), request_id: rid });
        sm.handle(UiEvent::UserCancel);

        assert_eq!(sm.streaming_text(), "");
    }

    #[test]
    fn streaming_text_cleared_on_mode_switch() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);

        sm.handle(UiEvent::StreamDelta { text: "partial".into(), request_id: rid });
        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));

        assert_eq!(sm.streaming_text(), "");
    }

    #[test]
    fn streaming_text_cleared_on_new_text() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);

        sm.handle(UiEvent::StreamDelta { text: "partial".into(), request_id: rid });
        start_processing(&mut sm, "new input");

        assert_eq!(sm.streaming_text(), "");
    }

    // === Image content tests ===

    fn image_only_content() -> ClipboardContent {
        ClipboardContent {
            text: None,
            images: vec![crate::images::ImageAttachment::stub(vec![0x89, 0x50, 0x4E, 0x47])],
            files: vec![],
        }
    }

    fn text_and_image_content() -> ClipboardContent {
        ClipboardContent {
            text: Some("caption".into()),
            images: vec![crate::images::ImageAttachment::stub(vec![0x89, 0x50, 0x4E, 0x47])],
            files: vec![],
        }
    }

    #[test]
    fn image_only_auto_switches_to_summarize() {
        let mut sm = new_sm(); // starts in Translate mode
        assert_eq!(sm.mode(), ProcessMode::Translate);

        let effects = sm.handle(UiEvent::ContentReady { content: image_only_content(), auto_copy: true });

        assert_eq!(sm.mode(), ProcessMode::Summarize);
        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(effects.iter().any(|e| matches!(
            e,
            UiEffect::SendProcess { mode: ProcessMode::Summarize, .. }
        )));
    }

    #[test]
    fn image_only_available_modes_are_the_image_consuming_ones() {
        let mut sm = new_sm();
        sm.handle(UiEvent::ContentReady { content: image_only_content(), auto_copy: true });

        assert_eq!(
            sm.available_modes(),
            &[ProcessMode::Summarize, ProcessMode::Explain, ProcessMode::Transcribe]
        );
    }

    #[test]
    fn image_only_keeps_explain_mode() {
        let mut sm = StateMachine::new(ProcessMode::Explain);
        sm.handle(UiEvent::ContentReady { content: image_only_content(), auto_copy: true });

        // Explain already consumes images, so it is not forced back to Summarize.
        assert_eq!(sm.mode(), ProcessMode::Explain);
    }

    #[test]
    fn image_only_allows_switch_to_explain() {
        let mut sm = new_sm();
        sm.handle(UiEvent::ContentReady { content: image_only_content(), auto_copy: true });
        assert_eq!(sm.mode(), ProcessMode::Summarize);

        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Explain));
        assert_eq!(sm.mode(), ProcessMode::Explain);
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    #[test]
    fn text_and_image_keeps_mode() {
        let mut sm = new_sm(); // Translate mode
        sm.handle(UiEvent::ContentReady { content: text_and_image_content(), auto_copy: true });

        assert_eq!(sm.mode(), ProcessMode::Translate);
        assert_eq!(sm.available_modes(), ProcessMode::ALL);
    }

    #[test]
    fn text_only_available_modes_all() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");

        // Transcribe re-expresses text as well as images, so no mode is gated out.
        assert_eq!(sm.available_modes(), ProcessMode::ALL);
    }

    #[test]
    fn image_only_keeps_transcribe_mode() {
        let mut sm = StateMachine::new(ProcessMode::Transcribe);
        sm.handle(UiEvent::ContentReady { content: image_only_content(), auto_copy: true });

        assert_eq!(sm.mode(), ProcessMode::Transcribe);
    }

    #[test]
    fn transcribe_survives_text_content() {
        // Transcribe accepts any medium, so text content never forces it aside.
        let mut sm = StateMachine::new(ProcessMode::Transcribe);
        sm.handle(UiEvent::ContentReady { content: text_and_image_content(), auto_copy: true });
        assert_eq!(sm.mode(), ProcessMode::Transcribe);

        let mut sm = StateMachine::new(ProcessMode::Transcribe);
        start_processing(&mut sm, "hello");
        assert_eq!(sm.mode(), ProcessMode::Transcribe);
    }

    #[test]
    fn mode_switch_to_transcribe_allowed_for_text_content() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");

        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Transcribe));
        assert_eq!(sm.mode(), ProcessMode::Transcribe);
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    #[test]
    fn image_only_allows_switch_to_transcribe() {
        let mut sm = new_sm();
        sm.handle(UiEvent::ContentReady { content: image_only_content(), auto_copy: true });

        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Transcribe));
        assert_eq!(sm.mode(), ProcessMode::Transcribe);
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    #[test]
    fn mode_switch_blocked_when_image_only() {
        let mut sm = new_sm();
        sm.handle(UiEvent::ContentReady { content: image_only_content(), auto_copy: true });
        assert_eq!(sm.mode(), ProcessMode::Summarize);

        // Try switching to Translate — should be blocked.
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Translate));
        assert!(effects.is_empty());
        assert_eq!(sm.mode(), ProcessMode::Summarize);
    }

    #[test]
    fn no_content_available_modes_empty() {
        let sm = new_sm();
        // No content loaded — all tabs should be disabled.
        assert!(sm.available_modes().is_empty());
    }

    // === Clipboard error edge cases ===

    #[test]
    fn clipboard_error_from_hidden_then_mode_switch_no_panic() {
        let mut sm = new_sm();
        assert_eq!(*sm.state(), OverlayState::Hidden);

        // Clipboard read fails (e.g. copy_and_read timeout) → Error with no original_content.
        let effects = sm.handle(UiEvent::ClipboardError("timeout".into()));
        assert_eq!(*sm.state(), OverlayState::Error("timeout".into()));
        assert!(effects.contains(&UiEffect::ShowWindow));

        // User switches mode from Error — should NOT transition to Processing
        // since there is no content to reprocess.
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        assert!(effects.is_empty());
        // State stays Error, not Processing.
        assert_eq!(*sm.state(), OverlayState::Error("timeout".into()));
    }

    #[test]
    fn clipboard_error_detached_requires_fresh_focus_cycle() {
        // #62: a clipboard error entered without focus (capture uses
        // ShowWindowNoActivate, so is_focused is false at entry) is detached —
        // a bare FocusLost is ignored until the user focuses it and leaves.
        // (In the real app the ShowWindow effect activates the window, firing
        // FocusGained, which re-arms has_been_focused; the pure state machine
        // doesn't simulate that, so it tests the detached entry directly.)
        let mut sm = new_sm();
        // Simulate a previous session where focus was gained, then lost → Hidden.
        start_processing(&mut sm, "hello");
        let rid = sm.current_request_id();
        sm.handle(UiEvent::WorkerResult {
            text: "done".into(),
            think_content: None,
            request_id: rid, incomplete: None });
        sm.handle(UiEvent::FocusGained);
        sm.handle(UiEvent::FocusLost);
        assert_eq!(*sm.state(), OverlayState::Hidden);

        // Clipboard error shows the error overlay, detached (no focus yet).
        sm.handle(UiEvent::ClipboardError("read failed".into()));
        assert_eq!(*sm.state(), OverlayState::Error("read failed".into()));

        // Bare FocusLost is ignored — the user has not focused the error yet.
        let effects = sm.handle(UiEvent::FocusLost);
        assert!(effects.is_empty());
        assert_eq!(*sm.state(), OverlayState::Error("read failed".into()));

        // A fresh focus → unfocus cycle dismisses it.
        sm.handle(UiEvent::FocusGained);
        let effects = sm.handle(UiEvent::FocusLost);
        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn worker_error_detached_requires_fresh_focus_cycle() {
        // #62: an error that lands while the user is in another app (detached,
        // is_focused == false) stays visible until the user focuses it and
        // leaves — mirrors the detached-Result behavior. Contrast with
        // error_with_focus_held_through_failure_hides_on_focus_loss.
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        // Focus was gained during Processing, then lost (user went elsewhere,
        // request stays alive per #17).
        sm.handle(UiEvent::FocusGained);
        sm.handle(UiEvent::FocusLost);

        // Error lands while detached.
        sm.handle(UiEvent::WorkerError { message: "boom".into(), request_id: rid });
        assert_eq!(*sm.state(), OverlayState::Error("boom".into()));

        // Re-fired FocusLost is ignored — user has not seen the error yet.
        let effects = sm.handle(UiEvent::FocusLost);
        assert!(effects.is_empty());
        assert_eq!(*sm.state(), OverlayState::Error("boom".into()));

        // Fresh focus → unfocus dismisses it.
        sm.handle(UiEvent::FocusGained);
        let effects = sm.handle(UiEvent::FocusLost);
        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    // === Thinking mode tests ===

    #[test]
    fn thinking_probe_result_updates_supported() {
        let mut sm = new_sm();
        assert!(!sm.thinking_supported());

        sm.handle(UiEvent::ThinkingProbeResult(true));
        assert!(sm.thinking_supported());

        sm.handle(UiEvent::ThinkingProbeResult(false));
        assert!(!sm.thinking_supported());
    }

    #[test]
    fn effective_thinking_mode_defaults_per_process_mode() {
        let mut sm = new_sm(); // Translate mode
        assert_eq!(sm.effective_thinking_mode(), ThinkingMode::NoThink);

        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Summarize));
        assert_eq!(sm.effective_thinking_mode(), ThinkingMode::Think);
    }

    #[test]
    fn change_thinking_mode_triggers_reprocess() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let _rid = last_request_id(&effects);

        // Change thinking to Think (default is NoThink for Translate).
        let effects = sm.handle(UiEvent::UserChangeThinkingMode(ThinkingMode::Think));
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendCancel)));
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { thinking_mode: ThinkingMode::Think, .. })));
        assert_eq!(sm.effective_thinking_mode(), ThinkingMode::Think);
    }

    #[test]
    fn change_thinking_mode_same_value_is_noop() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");

        // NoThink is already default for Translate — should be no-op.
        let effects = sm.handle(UiEvent::UserChangeThinkingMode(ThinkingMode::NoThink));
        assert!(effects.is_empty());
    }

    #[test]
    fn change_thinking_mode_from_result_reprocesses() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "ok".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        let effects = sm.handle(UiEvent::UserChangeThinkingMode(ThinkingMode::Think));
        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { thinking_mode: ThinkingMode::Think, .. })));
    }

    #[test]
    fn mode_thinking_cleared_on_content_ready() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");

        // Override thinking for Translate.
        sm.handle(UiEvent::UserChangeThinkingMode(ThinkingMode::Think));
        assert_eq!(sm.effective_thinking_mode(), ThinkingMode::Think);

        // New content ready — should reset to default.
        start_processing(&mut sm, "world");
        assert_eq!(sm.effective_thinking_mode(), ThinkingMode::NoThink);
    }

    #[test]
    fn thinking_mode_per_process_mode_independent() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");

        // Set Translate to Think.
        sm.handle(UiEvent::UserChangeThinkingMode(ThinkingMode::Think));
        assert_eq!(sm.effective_thinking_mode(), ThinkingMode::Think);

        // Switch to Summarize — should use Summarize's default (Think).
        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Summarize));
        assert_eq!(sm.effective_thinking_mode(), ThinkingMode::Think);

        // Switch back to Translate — override should still be active.
        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Translate));
        assert_eq!(sm.effective_thinking_mode(), ThinkingMode::Think);
    }

    #[test]
    fn think_started_cleared_on_error() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);

        sm.handle(UiEvent::ThinkStarted { request_id: rid });
        assert!(sm.think_started());

        sm.handle(UiEvent::WorkerError {
            message: "fail".into(),
            request_id: rid,
        });
        assert!(!sm.think_started());
    }

    #[test]
    fn rephrase_params_reset_on_content_ready() {
        let mut sm = new_sm();
        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        start_processing(&mut sm, "hello");

        // Change length.
        sm.handle(UiEvent::UserChangeRephraseLength(RephraseLength::Terse));
        assert_eq!(sm.rephrase_params().length, RephraseLength::Terse);

        // New content — should reset to default.
        start_processing(&mut sm, "world");
        assert_eq!(sm.rephrase_params().length, RephraseLength::default());
    }

    // === Auto-copy (single-tap vs double-tap) tests ===

    #[test]
    fn single_tap_no_auto_copy() {
        let mut sm = new_sm();
        let effects = sm.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only("hello".into()),
            auto_copy: false,
        });
        assert!(!sm.auto_copy());
        let rid = last_request_id(&effects);

        let effects = sm.handle(UiEvent::WorkerResult {
            text: "result".into(),
            think_content: None,
            request_id: rid, incomplete: None });
        assert_eq!(*sm.state(), OverlayState::Result("result".into()));
        assert!(!effects.iter().any(|e| matches!(e, UiEffect::WriteClipboard(_))));
    }

    #[test]
    fn single_tap_cached_no_auto_copy() {
        let mut sm = new_sm();
        // Single-tap session.
        let effects = sm.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only("hello".into()),
            auto_copy: false,
        });
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "translated".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        // Switch mode → reprocess.
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "rephrased".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        // Switch back to Translate — cache hit.
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Translate));
        assert_eq!(*sm.state(), OverlayState::Result("translated".into()));
        assert!(!effects.iter().any(|e| matches!(e, UiEffect::WriteClipboard(_))));
    }

    #[test]
    fn user_copy_in_result_state() {
        let mut sm = new_sm();
        let effects = sm.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only("hello".into()),
            auto_copy: false,
        });
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "result".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        let effects = sm.handle(UiEvent::UserCopy);
        assert!(effects.contains(&UiEffect::WriteClipboard("result".into())));
    }

    #[test]
    fn user_copy_not_in_result_state() {
        let mut sm = new_sm();
        // Hidden state.
        let effects = sm.handle(UiEvent::UserCopy);
        assert!(effects.is_empty());

        // Processing state.
        start_processing(&mut sm, "hello");
        let effects = sm.handle(UiEvent::UserCopy);
        assert!(effects.is_empty());
    }

    #[test]
    fn user_paste_in_result_state() {
        let mut sm = new_sm();
        let effects = sm.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only("hello".into()),
            auto_copy: true,
        });
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "result".into(),
            think_content: None,
            request_id: rid, incomplete: None });

        let effects = sm.handle(UiEvent::UserPaste);
        assert!(effects.contains(&UiEffect::WriteClipboard("result".into())));
        assert!(effects.contains(&UiEffect::HideWindow));
        assert!(effects.contains(&UiEffect::PasteClipboard));
        assert_eq!(sm.state(), &OverlayState::Hidden);
    }

    #[test]
    fn user_paste_not_in_result_state() {
        let mut sm = new_sm();
        // Hidden state.
        let effects = sm.handle(UiEvent::UserPaste);
        assert!(effects.is_empty());

        // Processing state.
        start_processing(&mut sm, "hello");
        let effects = sm.handle(UiEvent::UserPaste);
        assert!(effects.is_empty());
    }

    // === Capture (double-tap) ===

    #[test]
    fn capture_started_shows_spinner_non_activating() {
        let mut sm = new_sm();
        let effects = sm.handle(UiEvent::CaptureStarted { source: CaptureSource::Selection });

        assert_eq!(*sm.state(), OverlayState::Capturing);
        assert!(sm.auto_copy(), "capture is the double-tap (auto-copy) path");
        assert!(effects.contains(&UiEffect::CaptureMousePosition));
        assert!(effects.contains(&UiEffect::ShowWindowNoActivate));
        assert!(effects.contains(&UiEffect::StartCapture));
        assert!(effects.contains(&UiEffect::ResetAreas));
        // Must NOT activate the window (would steal focus and break the copy).
        assert!(!effects.contains(&UiEffect::ShowWindow));
    }

    #[test]
    fn capture_to_processing_on_content_ready() {
        let mut sm = new_sm();
        sm.handle(UiEvent::CaptureStarted { source: CaptureSource::Selection });

        let effects = sm.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only("captured".into()),
            auto_copy: true,
        });
        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
        // Now activates (re-acquires focus) for the result.
        assert!(effects.contains(&UiEffect::ShowWindow));
    }

    #[test]
    fn capture_to_error_on_clipboard_error() {
        let mut sm = new_sm();
        sm.handle(UiEvent::CaptureStarted { source: CaptureSource::Selection });

        let effects = sm.handle(UiEvent::ClipboardError("no text after copy".into()));
        assert_eq!(*sm.state(), OverlayState::Error("no text after copy".into()));
        assert!(effects.contains(&UiEffect::ShowWindow));
        // No stale content left behind after a failed capture.
        assert_eq!(sm.original_text(), None);
    }

    #[test]
    fn cancel_during_capture_hides_without_sendcancel() {
        let mut sm = new_sm();
        sm.handle(UiEvent::CaptureStarted { source: CaptureSource::Selection });

        let effects = sm.handle(UiEvent::UserCancel);
        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
        // No LLM request exists yet, so nothing to cancel on the worker.
        assert!(!effects.contains(&UiEffect::SendCancel));
    }

    #[test]
    fn close_during_capture_hides() {
        let mut sm = new_sm();
        sm.handle(UiEvent::CaptureStarted { source: CaptureSource::Selection });

        let effects = sm.handle(UiEvent::UserClose);
        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn focus_lost_during_capture_ignored() {
        let mut sm = new_sm();
        sm.handle(UiEvent::CaptureStarted { source: CaptureSource::Selection });
        // The capture overlay is shown non-activating, so it never gains focus;
        // a spurious FocusLost must not dismiss it.
        let effects = sm.handle(UiEvent::FocusLost);
        assert!(effects.is_empty());
        assert_eq!(*sm.state(), OverlayState::Capturing);
    }

    #[test]
    fn capture_started_clears_prior_content_and_cache() {
        let mut sm = new_sm();
        // Build up a Result with cached content.
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult { text: "translated".into(), think_content: None, request_id: rid, incomplete: None });
        assert!(sm.original_text().is_some());

        // A fresh double-tap capture drops the prior content (so a capture failure
        // can't leave stale content for a later mode switch to re-process).
        sm.handle(UiEvent::CaptureStarted { source: CaptureSource::Selection });
        assert_eq!(*sm.state(), OverlayState::Capturing);
        assert_eq!(sm.original_text(), None);
    }

    #[test]
    fn switch_mode_during_capture_is_noop() {
        let mut sm = new_sm();
        sm.handle(UiEvent::CaptureStarted { source: CaptureSource::Selection });
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        // No content yet — nothing to re-process; stays Capturing.
        assert!(effects.is_empty());
        assert_eq!(*sm.state(), OverlayState::Capturing);
    }

    // === Pin (keep-open) ===

    #[test]
    fn tap_results_start_unpinned_by_default() {
        // Default config: [ui].single_tap_pinned and double_tap_pinned both false.
        let mut sm = new_sm();
        sm.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only("hi".into()),
            auto_copy: false,
        });
        assert!(!sm.pinned(), "single-tap starts unpinned by default");

        let mut sm2 = new_sm();
        sm2.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only("hi".into()),
            auto_copy: true,
        });
        assert!(!sm2.pinned(), "double-tap starts unpinned by default");
    }

    #[test]
    fn toggle_pin_flips_state() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello"); // double-tap helper → unpinned
        assert!(!sm.pinned());
        sm.handle(UiEvent::UserTogglePin);
        assert!(sm.pinned());
        sm.handle(UiEvent::UserTogglePin);
        assert!(!sm.pinned());
    }

    #[test]
    fn pinned_overlay_survives_focus_loss() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello"); // double-tap → unpinned
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult { text: "out".into(), think_content: None, request_id: rid, incomplete: None });
        sm.handle(UiEvent::FocusGained);
        sm.handle(UiEvent::UserTogglePin); // pin it
        assert!(sm.pinned());

        let effects = sm.handle(UiEvent::FocusLost);
        assert!(effects.is_empty(), "pinned overlay must not auto-hide");
        assert_eq!(*sm.state(), OverlayState::Result("out".into()));
    }

    #[test]
    fn pinning_single_tap_result_keeps_it_open() {
        let mut sm = new_sm();
        let effects = sm.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only("hi".into()),
            auto_copy: false,
        });
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult { text: "out".into(), think_content: None, request_id: rid, incomplete: None });
        sm.handle(UiEvent::FocusGained);
        assert!(!sm.pinned(), "single-tap result starts unpinned by default");

        // User pins it → focus loss now keeps it open.
        sm.handle(UiEvent::UserTogglePin);
        let effects = sm.handle(UiEvent::FocusLost);
        assert!(effects.is_empty());
        assert_eq!(*sm.state(), OverlayState::Result("out".into()));
    }

    #[test]
    fn pin_resets_on_close_and_new_trigger() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");
        sm.handle(UiEvent::UserTogglePin);
        assert!(sm.pinned());

        sm.handle(UiEvent::UserClose);
        assert!(!sm.pinned(), "close resets pin");

        // New double-tap trigger also starts unpinned.
        sm.handle(UiEvent::CaptureStarted { source: CaptureSource::Selection });
        assert!(!sm.pinned());
    }

    // -- capture source tests (#50) --

    #[test]
    fn capture_source_tracks_trigger() {
        let mut sm = StateMachine::new(ProcessMode::Translate);
        // Double-tap path: source recorded at capture start.
        sm.handle(UiEvent::CaptureStarted { source: CaptureSource::Selection });
        assert_eq!(sm.capture_source(), CaptureSource::Selection);
        // A single-tap commit corrects the source via auto_copy = false
        // (CaptureStarted optimistically assumes the double-tap path).
        sm.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only("hello".into()),
            auto_copy: false,
        });
        assert_eq!(sm.capture_source(), CaptureSource::Clipboard);
        // Double-tap content keeps Selection.
        sm.handle(UiEvent::ContentReady {
            content: ClipboardContent::text_only("other".into()),
            auto_copy: true,
        });
        assert_eq!(sm.capture_source(), CaptureSource::Selection);
    }

    #[test]
    fn user_resize_pins_placement_until_the_next_trigger() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");
        assert!(!sm.user_repositioned());

        assert!(sm.handle(UiEvent::UserResize).is_empty());
        assert!(sm.user_repositioned(), "a resize anchors the window like a drag");

        start_processing(&mut sm, "again");
        assert!(!sm.user_repositioned(), "a new trigger re-centers");
    }

    // -- Revision rounds --

    fn revise(sm: &mut StateMachine, instruction: &str) -> Vec<UiEffect> {
        sm.handle(UiEvent::UserRevise(instruction.to_string()))
    }

    fn sent_revision(effects: &[UiEffect]) -> Option<Vec<crate::RevisionTurn>> {
        effects.iter().find_map(|e| match e {
            UiEffect::SendProcess { revision, .. } => Some(revision.clone()),
            _ => None,
        })
    }

    fn complete(sm: &mut StateMachine, effects: &[UiEffect], text: &str) -> Vec<UiEffect> {
        let id = last_request_id(effects);
        sm.handle(UiEvent::WorkerResult { text: text.into(), think_content: None, request_id: id, incomplete: None })
    }

    /// A base result, then one revision in flight.
    fn with_pending_revision(sm: &mut StateMachine) -> Vec<UiEffect> {
        let effects = start_processing(sm, "orig");
        complete(sm, &effects, "base");
        revise(sm, "shorter")
    }

    #[test]
    fn revise_sends_the_round_chain_and_enters_processing() {
        let mut sm = new_sm();
        let effects = with_pending_revision(&mut sm);
        assert!(matches!(sm.state(), OverlayState::Processing));
        assert!(sm.revising());
        let chain = sent_revision(&effects).expect("revision request");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].reply_before, "base");
        assert_eq!(chain[0].instruction, "shorter");
        // Not the base request: the chain is empty there.
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "orig");
        assert_eq!(sent_revision(&effects).unwrap().len(), 0);
    }

    #[test]
    fn revise_ignored_without_a_result_or_an_instruction() {
        let mut sm = new_sm();
        assert!(revise(&mut sm, "x").is_empty(), "hidden");
        start_processing(&mut sm, "orig");
        assert!(revise(&mut sm, "x").is_empty(), "processing");
        let effects = start_processing(&mut sm, "orig");
        complete(&mut sm, &effects, "base");
        assert!(revise(&mut sm, "   ").is_empty(), "blank instruction");
        assert!(matches!(sm.state(), OverlayState::Result(_)));
    }

    #[test]
    fn revised_result_is_cached_with_its_rounds_and_survives_a_mode_switch() {
        let mut sm = new_sm();
        let effects = with_pending_revision(&mut sm);
        let effects = complete(&mut sm, &effects, "base v2");
        assert_eq!(sm.state(), &OverlayState::Result("base v2".into()));
        assert!(!sm.revising());
        assert_eq!(sm.revision_instructions(), vec!["shorter"]);
        // Double-tap session: the revised text replaces the clipboard copy.
        assert!(effects.contains(&UiEffect::WriteClipboard("base v2".into())));
        // Another mode starts from scratch (no rounds), then coming back
        // restores the revised text and its rounds without a request.
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Summarize));
        assert_eq!(sent_revision(&effects).unwrap().len(), 0);
        assert!(sm.revision_instructions().is_empty());
        complete(&mut sm, &effects, "summary");
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Translate));
        assert!(sent_revision(&effects).is_none(), "served from cache");
        assert_eq!(sm.state(), &OverlayState::Result("base v2".into()));
        assert_eq!(sm.revision_instructions(), vec!["shorter"]);
    }

    #[test]
    fn second_revision_chains_on_the_revised_reply() {
        let mut sm = new_sm();
        let effects = with_pending_revision(&mut sm);
        complete(&mut sm, &effects, "base v2");
        let effects = revise(&mut sm, "formal");
        let chain = sent_revision(&effects).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!((chain[0].reply_before.as_str(), chain[0].instruction.as_str()), ("base", "shorter"));
        assert_eq!((chain[1].reply_before.as_str(), chain[1].instruction.as_str()), ("base v2", "formal"));
        complete(&mut sm, &effects, "base v3");
        assert_eq!(sm.revision_instructions(), vec!["shorter", "formal"]);
    }

    #[test]
    fn failed_revision_returns_to_the_previous_reply_with_a_notice() {
        let mut sm = new_sm();
        let effects = with_pending_revision(&mut sm);
        let id = last_request_id(&effects);
        let effects = sm.handle(UiEvent::WorkerError { message: "boom".into(), request_id: id });
        assert_eq!(sm.state(), &OverlayState::Result("base".into()));
        assert!(!sm.revising());
        assert!(sm.revision_instructions().is_empty());
        assert_eq!(sm.revision_error(), Some("boom"));
        assert_eq!(sm.take_failed_instruction(), Some("shorter".into()));
        assert_eq!(sm.take_failed_instruction(), None, "handed back once");
        assert!(!effects.contains(&UiEffect::HideWindow));
        // The next request clears the notice.
        revise(&mut sm, "again");
        assert_eq!(sm.revision_error(), None);
    }

    #[test]
    fn cancel_during_a_revision_restores_the_previous_reply() {
        let mut sm = new_sm();
        with_pending_revision(&mut sm);
        let effects = sm.handle(UiEvent::UserCancel);
        assert!(effects.contains(&UiEffect::SendCancel));
        assert!(!effects.contains(&UiEffect::HideWindow), "the reply stays on screen");
        assert_eq!(sm.state(), &OverlayState::Result("base".into()));
        assert!(!sm.revising());
        // Close (Escape from the base request) still hides as before.
        let mut sm = new_sm();
        with_pending_revision(&mut sm);
        let effects = sm.handle(UiEvent::UserClose);
        assert!(effects.contains(&UiEffect::HideWindow));
        assert!(matches!(sm.state(), OverlayState::Hidden));
    }

    #[test]
    fn undo_pops_the_last_round_and_restores_its_reply() {
        let mut sm = new_sm();
        assert!(sm.handle(UiEvent::UserUndoRevision).is_empty(), "nothing to undo");
        let effects = with_pending_revision(&mut sm);
        complete(&mut sm, &effects, "base v2");
        let effects = sm.handle(UiEvent::UserUndoRevision);
        assert_eq!(sm.state(), &OverlayState::Result("base".into()));
        assert!(sm.revision_instructions().is_empty());
        assert!(effects.contains(&UiEffect::WriteClipboard("base".into())), "double-tap: clipboard follows");
        assert!(sm.handle(UiEvent::UserUndoRevision).is_empty());
        // The undone state is what a mode round-trip restores.
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Summarize));
        complete(&mut sm, &effects, "summary");
        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Translate));
        assert_eq!(sm.state(), &OverlayState::Result("base".into()));
    }

    #[test]
    fn retry_re_runs_the_last_round() {
        let mut sm = new_sm();
        let effects = with_pending_revision(&mut sm);
        complete(&mut sm, &effects, "base v2");
        let effects = sm.handle(UiEvent::UserRetry);
        let chain = sent_revision(&effects).expect("re-sent");
        assert_eq!(chain.len(), 1);
        assert_eq!((chain[0].reply_before.as_str(), chain[0].instruction.as_str()), ("base", "shorter"));
        complete(&mut sm, &effects, "base v2b");
        assert_eq!(sm.revision_instructions(), vec!["shorter"]);
    }

    #[test]
    fn new_content_and_a_model_switch_reset_the_rounds() {
        let mut sm = new_sm();
        let effects = with_pending_revision(&mut sm);
        complete(&mut sm, &effects, "base v2");
        let effects = start_processing(&mut sm, "other");
        assert_eq!(sent_revision(&effects).unwrap().len(), 0);
        assert!(sm.revision_instructions().is_empty());
        complete(&mut sm, &effects, "base");
        let effects = revise(&mut sm, "shorter");
        complete(&mut sm, &effects, "base v2");
        let effects = sm.handle(UiEvent::UserSelectModel(1));
        assert_eq!(sent_revision(&effects).unwrap().len(), 0);
        assert!(sm.revision_instructions().is_empty());
    }
}
