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
    Processing,
    Result(String),
    Error(String),
}

impl OverlayState {
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::Hidden => "Hidden",
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
    HideWindow,
    CaptureMousePosition,
    /// Reset egui Area stored sizing (needed on state variant change).
    ResetAreas,
    /// Simulate paste (Cmd+V / Ctrl+V) into the previously focused app.
    PasteClipboard,
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
        }
    }

    // -- Accessors --

    pub fn state(&self) -> &OverlayState {
        &self.state
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
        };

        self.check_invariants();
        effects
    }

    // -- Private transition handlers --

    fn on_content_ready(&mut self, content: ClipboardContent, auto_copy: bool) -> Vec<UiEffect> {
        let old_state = self.state.clone();

        // Image-only content: auto-switch to Summarize.
        if content.text.is_none() && content.has_images() {
            self.mode = ProcessMode::Summarize;
        }

        self.original_content = Some(content.clone());
        self.cache.clear();
        self.mode_thinking.clear();
        self.rephrase_params = RephraseParams::default();
        self.streaming_text.clear();
        self.think_started = false;
        self.think_content = None;
        self.auto_copy = auto_copy;
        self.next_request_id += 1;
        self.current_request_id = self.next_request_id;
        self.state = OverlayState::Processing;
        self.user_repositioned = false;
        self.has_been_focused = false;

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
    }

    fn on_close(&mut self) -> Vec<UiEffect> {
        if matches!(self.state, OverlayState::Hidden) {
            return vec![];
        }
        self.reset_to_hidden();
        vec![UiEffect::HideWindow]
    }

    fn on_cancel(&mut self) -> Vec<UiEffect> {
        if !matches!(self.state, OverlayState::Processing) {
            return vec![];
        }
        self.reset_to_hidden();
        vec![UiEffect::SendCancel, UiEffect::HideWindow]
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
        // Cache key is computed after setting the new mode.
        let key = self.cache_key();

        match &self.state {
            OverlayState::Processing => {
                if let Some((cached_text, cached_think)) = self.cache.get(&key).cloned() {
                    // Cache hit: cancel in-flight, return cached result.
                    self.streaming_text.clear();
                    self.think_started = false;
                    let mut effects = self.apply_cached_result(cached_text, cached_think);
                    effects.insert(0, UiEffect::SendCancel);
                    effects
                } else if let Some(content) = self.original_content.clone() {
                    // Cache miss: cancel current, re-send with new mode.
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
                    // Defensive: no content to reprocess — just cancel.
                    vec![UiEffect::SendCancel]
                }
            }
            OverlayState::Result(_) | OverlayState::Error(_) => {
                if let Some((cached_text, cached_think)) = self.cache.get(&key).cloned() {
                    // Cache hit: return cached result directly.
                    self.apply_cached_result(cached_text, cached_think)
                } else if let Some(content) = self.original_content.clone() {
                    // Cache miss: re-process with new mode.
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
                    // No content to reprocess (e.g. error from clipboard read failure).
                    vec![]
                }
            }
            OverlayState::Hidden => vec![],
        }
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

    fn on_clipboard_error(&mut self, msg: String) -> Vec<UiEffect> {
        // Must NOT emit WriteClipboard to avoid infinite recursion.
        self.state = OverlayState::Error(msg);
        // Reset focus tracking so the newly shown error window doesn't
        // immediately auto-hide from a stale has_been_focused flag.
        self.has_been_focused = false;
        vec![UiEffect::ResetAreas, UiEffect::ShowWindow]
    }

    /// Cache key for the current mode + rephrase params + thinking mode combination.
    fn cache_key(&self) -> String {
        format!(
            "{}|{:?}",
            self.mode.system_prompt(self.rephrase_params, false),
            self.effective_thinking_mode(),
        )
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
        self.on_params_changed()
    }

    /// Re-process or serve from cache when rephrase params change (Rephrase mode only).
    fn on_rephrase_params_changed(&mut self) -> Vec<UiEffect> {
        if self.mode != ProcessMode::Rephrase {
            return vec![];
        }
        self.on_params_changed()
    }

    /// Common logic for re-processing after params (rephrase or thinking) change.
    fn on_params_changed(&mut self) -> Vec<UiEffect> {
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
            OverlayState::Hidden => vec![],
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
}
