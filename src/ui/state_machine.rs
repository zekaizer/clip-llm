//! Pure UI state machine — no egui dependency.
//!
//! Receives [`UiEvent`]s and returns [`UiEffect`]s that the adapter layer
//! (OverlayApp) must execute.  This separation makes the state transition
//! logic fully unit-testable.

use std::collections::HashMap;

use crate::{ClipboardContent, ProcessMode, RephraseLength, RephraseParams, RephraseStyle, ThinkingMode};

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

// ---------------------------------------------------------------------------
// UiEvent / UiEffect
// ---------------------------------------------------------------------------

/// Events fed into the state machine by the adapter layer.
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// Double-tap pressed: begin capturing the current selection on a background
    /// thread. Shows the overlay (non-activating) with a spinner before any I/O.
    CaptureStarted,
    /// Clipboard content ready for processing.
    /// `auto_copy`: when true, auto-copy the result to clipboard (double-tap behavior).
    ContentReady { content: ClipboardContent, auto_copy: bool },
    /// Worker completed successfully.
    WorkerResult { text: String, think_content: Option<String>, request_id: u64 },
    /// Worker detected a think block beginning (streaming only).
    ThinkStarted { request_id: u64 },
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
    /// User started dragging the overlay.
    UserStartDrag,
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
}

/// Side effects that the adapter must execute after a state transition.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEffect {
    SendProcess {
        content: ClipboardContent,
        mode: ProcessMode,
        rephrase_params: RephraseParams,
        thinking_mode: ThinkingMode,
        request_id: u64,
    },
    SendCancel,
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
    /// Result cache: maps cache_key → (text, think_content).
    /// Valid only for the current original content.
    cache: HashMap<String, (String, Option<String>)>,
    /// Accumulated visible streaming text (displayed during Processing).
    streaming_text: String,
    /// True once a think block has started during the current streaming request.
    think_started: bool,
    /// Think block content for the current mode (set on WorkerResult).
    think_content: Option<String>,
    /// Whether the current session should auto-copy results to clipboard.
    /// Set by ContentReady (true for double-tap, false for single-tap).
    auto_copy: bool,
    /// When true, the overlay never auto-hides on focus loss (the user pinned it).
    /// Reset on every new trigger and on close.
    pinned: bool,
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
            cache: HashMap::new(),
            streaming_text: String::new(),
            think_started: false,
            think_content: None,
            auto_copy: false,
            pinned: false,
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

    pub fn think_content(&self) -> Option<&str> {
        self.think_content.as_deref()
    }

    pub fn user_repositioned(&self) -> bool {
        self.user_repositioned
    }

    pub fn auto_copy(&self) -> bool {
        self.auto_copy
    }

    pub fn current_request_id(&self) -> u64 {
        self.current_request_id
    }

    pub fn variant_name(&self) -> &'static str {
        self.state.variant_name()
    }

    /// Modes available for the current content.
    /// - No content: no modes available (tabs disabled).
    /// - Image-only: Summarize only.
    /// - Text (with or without images): all modes.
    pub fn available_modes(&self) -> &[ProcessMode] {
        match &self.original_content {
            None => &[],
            Some(content) if content.text.is_none() && content.has_images() => {
                &[ProcessMode::Summarize]
            }
            Some(_) => ProcessMode::ALL,
        }
    }

    #[cfg(test)]
    pub fn original_text(&self) -> Option<&str> {
        self.original_content.as_ref().and_then(|c| c.text.as_deref())
    }

    // -- Core event handler --

    pub fn handle(&mut self, event: UiEvent) -> Vec<UiEffect> {
        let effects = match event {
            UiEvent::CaptureStarted => self.on_capture_started(),
            UiEvent::ContentReady { content, auto_copy } => self.on_content_ready(content, auto_copy),
            UiEvent::WorkerResult { text, think_content, request_id } => {
                self.on_worker_result(text, think_content, request_id)
            }
            UiEvent::ThinkStarted { request_id } => self.on_think_started(request_id),
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
            UiEvent::ThinkingProbeResult(supported) => {
                self.thinking_supported = supported;
                vec![]
            }
            UiEvent::UserStartDrag => {
                self.user_repositioned = true;
                vec![]
            }
            UiEvent::FocusGained => {
                self.has_been_focused = true;
                vec![]
            }
            UiEvent::StreamDelta { text, request_id } => {
                self.on_stream_delta(text, request_id)
            }
            UiEvent::FocusLost => self.on_focus_lost(),
            UiEvent::ClipboardError(msg) => self.on_clipboard_error(msg),
            UiEvent::UserCopy => self.on_user_copy(),
            UiEvent::UserPaste => self.on_user_paste(),
            UiEvent::UserTogglePin => {
                self.pinned = !self.pinned;
                vec![]
            }
            UiEvent::UserRetry => self.on_user_retry(),
        };

        self.check_invariants();
        effects
    }

    // -- Private transition handlers --

    fn on_capture_started(&mut self) -> Vec<UiEffect> {
        let old_state = self.state.clone();

        // A double-tap always starts a fresh capture. Drop any prior content and
        // cached results so a capture *failure* (→ Error) can never leave stale
        // content that a later mode switch would re-process. The content is unknown
        // until the background capture completes, so we cannot key off it here.
        self.original_content = None;
        self.cache.clear();
        self.mode_thinking.clear();
        self.rephrase_params = RephraseParams::default();
        self.streaming_text.clear();
        self.think_started = false;
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

        // Image-only content: auto-switch to Summarize.
        if content.text.is_none() && content.has_images() {
            self.mode = ProcessMode::Summarize;
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
        if content_changed {
            self.cache.clear();
            self.mode_thinking.clear();
            self.rephrase_params = RephraseParams::default();
        }
        self.streaming_text.clear();
        self.think_started = false;
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
        self.streaming_text.push_str(&text);
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
        vec![]
    }

    fn on_worker_result(&mut self, text: String, think_content: Option<String>, request_id: u64) -> Vec<UiEffect> {
        if request_id != self.current_request_id {
            return vec![];
        }
        if !matches!(self.state, OverlayState::Processing) {
            return vec![];
        }
        self.streaming_text.clear();
        self.think_started = false;
        self.think_content = think_content.clone();
        self.cache.insert(self.cache_key(), (text.clone(), think_content));
        self.state = OverlayState::Result(text.clone());
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
        self.state = OverlayState::Error(message);
        // Reset focus tracking so the newly shown error window doesn't
        // immediately auto-hide from a stale has_been_focused flag carried
        // over from the Processing phase (mirrors on_clipboard_error).
        self.has_been_focused = false;
        vec![UiEffect::ResetAreas]
    }

    /// Resets all transient state and transitions to Hidden.
    fn reset_to_hidden(&mut self) {
        self.state = OverlayState::Hidden;
        self.original_content = None;
        self.cache.clear();
        self.streaming_text.clear();
        self.think_started = false;
        self.think_content = None;
        self.has_been_focused = false;
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
        self.reprocess_or_cache()
    }

    /// Applies a cached result: updates think_content and state,
    /// returns [WriteClipboard, ResetAreas].
    fn apply_cached_result(&mut self, text: String, think_content: Option<String>) -> Vec<UiEffect> {
        self.think_content = think_content;
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
        // Not pinned: auto-hide. Processing additionally cancels the in-flight
        // request, otherwise it runs to completion and its response is silently
        // dropped (on_worker_result rejects results once we leave Processing).
        // (Single-tap results start pinned via on_content_ready, so they are
        // already handled by the guard above and never reach here.)
        if matches!(self.state, OverlayState::Processing) {
            self.reset_to_hidden();
            return vec![UiEffect::SendCancel, UiEffect::HideWindow];
        }
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

    fn on_clipboard_error(&mut self, msg: String) -> Vec<UiEffect> {
        // Must NOT emit WriteClipboard to avoid infinite recursion.
        self.state = OverlayState::Error(msg);
        // Reset focus tracking so the newly shown error window doesn't
        // immediately auto-hide from a stale has_been_focused flag.
        self.has_been_focused = false;
        vec![UiEffect::ResetAreas, UiEffect::ShowWindow]
    }

    /// Whether the loaded content is image-only (no usable text). This selects the
    /// image-specific Summarize prompt in the API client, so it is part of the
    /// cache identity. Mirrors `LlmClient`'s `image_only` derivation for every case
    /// that is actually cached (an image-only request with no vision support errors
    /// out before producing a cacheable result).
    fn is_image_only(&self) -> bool {
        self.original_content
            .as_ref()
            .is_some_and(|c| c.text.is_none() && c.has_images())
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
            ProcessMode::Summarize => {
                format!("summarize|{}|{thinking:?}", self.is_image_only())
            }
            ProcessMode::Rephrase => format!(
                "rephrase|{:?}|{:?}|{thinking:?}",
                self.rephrase_params.style, self.rephrase_params.length,
            ),
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
        self.reprocess_or_cache()
    }

    /// Re-process or serve from cache when rephrase params change (Rephrase mode only).
    fn on_rephrase_params_changed(&mut self) -> Vec<UiEffect> {
        if self.mode != ProcessMode::Rephrase {
            return vec![];
        }
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
                if let Some((cached_text, cached_think)) = self.cache.get(&key).cloned() {
                    self.streaming_text.clear();
                    self.think_started = false;
                    let mut effects = self.apply_cached_result(cached_text, cached_think);
                    effects.insert(0, UiEffect::SendCancel);
                    effects
                } else if let Some(content) = self.original_content.clone() {
                    self.streaming_text.clear();
                    self.think_started = false;
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
                        },
                    ]
                } else {
                    vec![UiEffect::SendCancel]
                }
            }
            OverlayState::Result(_) | OverlayState::Error(_) => {
                if let Some((cached_text, cached_think)) = self.cache.get(&key).cloned() {
                    self.apply_cached_result(cached_text, cached_think)
                } else if let Some(content) = self.original_content.clone() {
                    // Clear any partial stream left over from a request that
                    // errored mid-stream, so the new Processing view starts clean.
                    self.streaming_text.clear();
                    self.think_started = false;
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
            request_id: rid,
        });

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
            request_id: rid,
        });

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
        sm.handle(UiEvent::CaptureStarted);
        assert_eq!(*sm.state(), OverlayState::Capturing);

        let effects = sm.handle(UiEvent::UserCancel);

        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::CancelCapture));
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn close_during_capturing_aborts_capture() {
        let mut sm = new_sm();
        sm.handle(UiEvent::CaptureStarted);
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
            request_id: rid,
        });

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
            request_id: rid1,
        });

        assert!(effects.is_empty());
        assert_eq!(*sm.state(), OverlayState::Processing);

        // Current response works.
        let effects = sm.handle(UiEvent::WorkerResult {
            text: "current".into(),
            think_content: None,
            request_id: rid2,
        });

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
            request_id: rid,
        });

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
        start_processing(&mut sm, "hello");
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
    fn focus_lost_during_processing_cancels_request() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");
        sm.handle(UiEvent::FocusGained);

        let effects = sm.handle(UiEvent::FocusLost);

        assert_eq!(*sm.state(), OverlayState::Hidden);
        // Must cancel the in-flight request, not just hide.
        assert!(effects.contains(&UiEffect::SendCancel));
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
        sm.handle(UiEvent::WorkerResult { text: "translated".into(), think_content: None, request_id: rid });
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
        sm.handle(UiEvent::WorkerResult { text: "translated".into(), think_content: None, request_id: rid });
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
            request_id: rid,
        });

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
            request_id: rid,
        });
        assert!(sm.original_text().is_some());

        // Result -> Processing (mode switch)
        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        let rid2 = sm.current_request_id();
        assert!(sm.original_text().is_some());

        // Processing -> Result
        sm.handle(UiEvent::WorkerResult {
            text: "corrected".into(),
            think_content: None,
            request_id: rid2,
        });

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
            request_id: rid,
        });
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
            request_id: rid,
        });
        // Switch to Correct → Result
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "corrected".into(),
            think_content: None,
            request_id: rid,
        });
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
            request_id: rid,
        });
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
            request_id: rid,
        });
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
            request_id: rid,
        });
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
        sm.handle(UiEvent::WorkerResult { text: "translated".into(), think_content: None, request_id: rid });
        // Rephrase → cache "rephrased".
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult { text: "rephrased".into(), think_content: None, request_id: rid });
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
        sm.handle(UiEvent::WorkerResult { text: "translated2".into(), think_content: None, request_id: rid });

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
            request_id: rid,
        });

        // Close overlay → cache cleared
        sm.handle(UiEvent::UserClose);
        assert_eq!(*sm.state(), OverlayState::Hidden);

        // Re-enter with same text: should go to Processing (not cached)
        let effects = sm.handle(UiEvent::ContentReady { content: ClipboardContent::text_only("hello".into()), auto_copy: true });
        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    #[test]
    fn image_only_summarize_does_not_share_cache_key_with_text() {
        use std::sync::Arc;
        // The API client uses the image-specific Summarize prompt for image-only
        // content, so its result must not collide with a text Summarize result.
        let mut sm = StateMachine::new(ProcessMode::Summarize);

        // Text Summarize → Result, cached under the text key.
        let e = start_processing(&mut sm, "hello");
        let rid = last_request_id(&e);
        sm.handle(UiEvent::WorkerResult { text: "text summary".into(), think_content: None, request_id: rid });
        let text_key = sm.cache_key();

        // New image-only content (stays Summarize) → distinct key.
        let e = sm.handle(UiEvent::ContentReady {
            content: ClipboardContent { text: None, images: vec![Arc::new(vec![0x89, 0x50])] },
            auto_copy: true,
        });
        let rid = last_request_id(&e);
        let image_key = sm.cache_key();
        assert_ne!(text_key, image_key, "image-only and text Summarize must key differently");

        sm.handle(UiEvent::WorkerResult { text: "image summary".into(), think_content: None, request_id: rid });
        assert_eq!(*sm.state(), OverlayState::Result("image summary".into()));
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
            request_id: rid,
        });

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
            request_id: new_rid,
        });
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
            request_id: rid,
        });

        // Summarize result cached.
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Summarize));
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "summary".into(),
            think_content: None,
            request_id: rid,
        });

        // Retry Summarize, complete it, then switch back to Translate:
        // the Translate cache entry must still be served without a request.
        let effects = sm.handle(UiEvent::UserRetry);
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "summary v2".into(),
            think_content: None,
            request_id: rid,
        });
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Translate));
        assert_eq!(*sm.state(), OverlayState::Result("translated".into()));
        assert!(!effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }

    // === Streaming text ===

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
        sm.handle(UiEvent::WorkerResult { text: "done".into(), think_content: None, request_id: rid });

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

        sm.handle(UiEvent::WorkerResult { text: "done".into(), think_content: None, request_id: rid });
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
            images: vec![std::sync::Arc::new(vec![0x89, 0x50, 0x4E, 0x47])],
        }
    }

    fn text_and_image_content() -> ClipboardContent {
        ClipboardContent {
            text: Some("caption".into()),
            images: vec![std::sync::Arc::new(vec![0x89, 0x50, 0x4E, 0x47])],
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
    fn image_only_available_modes_only_summarize() {
        let mut sm = new_sm();
        sm.handle(UiEvent::ContentReady { content: image_only_content(), auto_copy: true });

        assert_eq!(sm.available_modes(), &[ProcessMode::Summarize]);
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

        assert_eq!(sm.available_modes(), ProcessMode::ALL);
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
    fn clipboard_error_resets_has_been_focused() {
        let mut sm = new_sm();
        // Simulate a previous session where focus was gained.
        start_processing(&mut sm, "hello");
        sm.handle(UiEvent::FocusGained);

        // Focus lost → Hidden.
        sm.handle(UiEvent::FocusLost);
        assert_eq!(*sm.state(), OverlayState::Hidden);

        // Clipboard error shows error overlay.
        sm.handle(UiEvent::ClipboardError("read failed".into()));
        assert_eq!(*sm.state(), OverlayState::Error("read failed".into()));

        // FocusLost should be ignored because has_been_focused was reset.
        let effects = sm.handle(UiEvent::FocusLost);
        assert!(effects.is_empty());
        assert_eq!(*sm.state(), OverlayState::Error("read failed".into()));
    }

    #[test]
    fn worker_error_resets_has_been_focused() {
        let mut sm = new_sm();
        let effects = start_processing(&mut sm, "hello");
        let rid = last_request_id(&effects);
        // Focus was gained during the Processing phase.
        sm.handle(UiEvent::FocusGained);

        // Worker error transitions to Error.
        sm.handle(UiEvent::WorkerError { message: "boom".into(), request_id: rid });
        assert_eq!(*sm.state(), OverlayState::Error("boom".into()));

        // FocusLost must be ignored because has_been_focused was reset, so the
        // user can read the error before it auto-hides.
        let effects = sm.handle(UiEvent::FocusLost);
        assert!(effects.is_empty());
        assert_eq!(*sm.state(), OverlayState::Error("boom".into()));
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
            request_id: rid,
        });

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
            request_id: rid,
        });
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
            request_id: rid,
        });

        // Switch mode → reprocess.
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Rephrase));
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "rephrased".into(),
            think_content: None,
            request_id: rid,
        });

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
            request_id: rid,
        });

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
            request_id: rid,
        });

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
        let effects = sm.handle(UiEvent::CaptureStarted);

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
        sm.handle(UiEvent::CaptureStarted);

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
        sm.handle(UiEvent::CaptureStarted);

        let effects = sm.handle(UiEvent::ClipboardError("no text after copy".into()));
        assert_eq!(*sm.state(), OverlayState::Error("no text after copy".into()));
        assert!(effects.contains(&UiEffect::ShowWindow));
        // No stale content left behind after a failed capture.
        assert_eq!(sm.original_text(), None);
    }

    #[test]
    fn cancel_during_capture_hides_without_sendcancel() {
        let mut sm = new_sm();
        sm.handle(UiEvent::CaptureStarted);

        let effects = sm.handle(UiEvent::UserCancel);
        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
        // No LLM request exists yet, so nothing to cancel on the worker.
        assert!(!effects.contains(&UiEffect::SendCancel));
    }

    #[test]
    fn close_during_capture_hides() {
        let mut sm = new_sm();
        sm.handle(UiEvent::CaptureStarted);

        let effects = sm.handle(UiEvent::UserClose);
        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert!(effects.contains(&UiEffect::HideWindow));
    }

    #[test]
    fn focus_lost_during_capture_ignored() {
        let mut sm = new_sm();
        sm.handle(UiEvent::CaptureStarted);
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
        sm.handle(UiEvent::WorkerResult { text: "translated".into(), think_content: None, request_id: rid });
        assert!(sm.original_text().is_some());

        // A fresh double-tap capture drops the prior content (so a capture failure
        // can't leave stale content for a later mode switch to re-process).
        sm.handle(UiEvent::CaptureStarted);
        assert_eq!(*sm.state(), OverlayState::Capturing);
        assert_eq!(sm.original_text(), None);
    }

    #[test]
    fn switch_mode_during_capture_is_noop() {
        let mut sm = new_sm();
        sm.handle(UiEvent::CaptureStarted);
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
        sm.handle(UiEvent::WorkerResult { text: "out".into(), think_content: None, request_id: rid });
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
        sm.handle(UiEvent::WorkerResult { text: "out".into(), think_content: None, request_id: rid });
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
        sm.handle(UiEvent::CaptureStarted);
        assert!(!sm.pinned());
    }
}
