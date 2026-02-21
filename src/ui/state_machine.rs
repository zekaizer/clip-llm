//! Pure UI state machine — no egui dependency.
//!
//! Receives [`UiEvent`]s and returns [`UiEffect`]s that the adapter layer
//! (OverlayApp) must execute.  This separation makes the state transition
//! logic fully unit-testable.

use std::collections::HashMap;

use crate::ProcessMode;

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
    /// Clipboard text ready for processing.
    TextReady(String),
    /// Worker completed successfully.
    WorkerResult { text: String, request_id: u64 },
    /// Worker reported an error.
    WorkerError { message: String, request_id: u64 },
    /// User pressed close / Escape.
    UserClose,
    /// User pressed cancel during processing.
    UserCancel,
    /// User switched processing mode via tab bar.
    UserSwitchMode(ProcessMode),
    /// User started dragging the overlay.
    UserStartDrag,
    /// Window lost focus (after having been focused at least once).
    FocusLost,
    /// Clipboard write failed (feedback from effect execution).
    ClipboardWriteError(String),
}

/// Side effects that the adapter must execute after a state transition.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEffect {
    SendProcess {
        text: String,
        mode: ProcessMode,
        request_id: u64,
    },
    SendCancel,
    WriteClipboard(String),
    ShowWindow,
    HideWindow,
    CaptureMousePosition,
    /// Reset egui Area stored sizing (needed on state variant change).
    ResetAreas,
}

// ---------------------------------------------------------------------------
// StateMachine
// ---------------------------------------------------------------------------

pub struct StateMachine {
    state: OverlayState,
    mode: ProcessMode,
    /// Original input text, retained for re-processing on mode switch.
    original_text: Option<String>,
    /// Monotonically increasing counter for request identification.
    next_request_id: u64,
    /// The request_id of the currently active request.
    current_request_id: u64,
    /// True after the user drags the overlay; suppresses auto-repositioning.
    user_repositioned: bool,
    /// True once the window has received focus after show_window.
    has_been_focused: bool,
    /// Per-mode result cache, valid only for the current original_text.
    mode_cache: HashMap<ProcessMode, String>,
}

impl StateMachine {
    pub fn new(mode: ProcessMode) -> Self {
        Self {
            state: OverlayState::Hidden,
            mode,
            original_text: None,
            next_request_id: 0,
            current_request_id: 0,
            user_repositioned: false,
            has_been_focused: false,
            mode_cache: HashMap::new(),
        }
    }

    // -- Accessors --

    pub fn state(&self) -> &OverlayState {
        &self.state
    }

    pub fn mode(&self) -> ProcessMode {
        self.mode
    }

    pub fn user_repositioned(&self) -> bool {
        self.user_repositioned
    }

    pub fn current_request_id(&self) -> u64 {
        self.current_request_id
    }

    pub fn variant_name(&self) -> &'static str {
        self.state.variant_name()
    }

    /// Call when the adapter detects window focus gained.
    pub fn set_focused(&mut self) {
        self.has_been_focused = true;
    }

    /// Call when the user starts dragging the overlay.
    pub fn set_user_repositioned(&mut self) {
        self.user_repositioned = true;
    }

    /// Set the processing mode (used by diagnostics scenario injection).
    pub fn set_mode(&mut self, mode: ProcessMode) {
        self.mode = mode;
    }

    #[cfg(test)]
    pub fn original_text(&self) -> Option<&str> {
        self.original_text.as_deref()
    }

    // -- Core event handler --

    pub fn handle(&mut self, event: UiEvent) -> Vec<UiEffect> {
        let effects = match event {
            UiEvent::TextReady(text) => self.on_text_ready(text),
            UiEvent::WorkerResult { text, request_id } => {
                self.on_worker_result(text, request_id)
            }
            UiEvent::WorkerError {
                message,
                request_id,
            } => self.on_worker_error(message, request_id),
            UiEvent::UserClose => self.on_close(),
            UiEvent::UserCancel => self.on_cancel(),
            UiEvent::UserSwitchMode(mode) => self.on_switch_mode(mode),
            UiEvent::UserStartDrag => {
                self.user_repositioned = true;
                vec![]
            }
            UiEvent::FocusLost => self.on_focus_lost(),
            UiEvent::ClipboardWriteError(msg) => self.on_clipboard_write_error(msg),
        };

        self.check_invariants();
        effects
    }

    // -- Private transition handlers --

    fn on_text_ready(&mut self, text: String) -> Vec<UiEffect> {
        let old_state = self.state.clone();
        self.original_text = Some(text.clone());
        self.mode_cache.clear();
        self.next_request_id += 1;
        self.current_request_id = self.next_request_id;
        self.state = OverlayState::Processing;
        self.user_repositioned = false;
        self.has_been_focused = false;

        let mut effects = vec![
            UiEffect::CaptureMousePosition,
            UiEffect::SendProcess {
                text,
                mode: self.mode,
                request_id: self.current_request_id,
            },
        ];
        if !old_state.same_variant(&self.state) {
            effects.push(UiEffect::ResetAreas);
        }
        effects.push(UiEffect::ShowWindow);
        effects
    }

    fn on_worker_result(&mut self, text: String, request_id: u64) -> Vec<UiEffect> {
        if request_id != self.current_request_id {
            return vec![];
        }
        if !matches!(self.state, OverlayState::Processing) {
            return vec![];
        }
        self.mode_cache.insert(self.mode, text.clone());
        self.state = OverlayState::Result(text.clone());
        vec![
            UiEffect::WriteClipboard(text),
            UiEffect::ResetAreas,
            UiEffect::ShowWindow,
        ]
    }

    fn on_worker_error(&mut self, message: String, request_id: u64) -> Vec<UiEffect> {
        if request_id != self.current_request_id {
            return vec![];
        }
        if !matches!(self.state, OverlayState::Processing) {
            return vec![];
        }
        self.state = OverlayState::Error(message);
        vec![UiEffect::ResetAreas, UiEffect::ShowWindow]
    }

    fn on_close(&mut self) -> Vec<UiEffect> {
        if matches!(self.state, OverlayState::Hidden) {
            return vec![];
        }
        self.state = OverlayState::Hidden;
        self.original_text = None;
        self.mode_cache.clear();
        self.has_been_focused = false;
        vec![UiEffect::HideWindow]
    }

    fn on_cancel(&mut self) -> Vec<UiEffect> {
        if !matches!(self.state, OverlayState::Processing) {
            return vec![];
        }
        self.state = OverlayState::Hidden;
        self.original_text = None;
        self.mode_cache.clear();
        self.has_been_focused = false;
        vec![UiEffect::SendCancel, UiEffect::HideWindow]
    }

    fn on_switch_mode(&mut self, new_mode: ProcessMode) -> Vec<UiEffect> {
        if self.mode == new_mode {
            return vec![];
        }
        self.mode = new_mode;

        match &self.state {
            OverlayState::Processing => {
                if let Some(cached) = self.mode_cache.get(&new_mode).cloned() {
                    // Cache hit: cancel in-flight, return cached result.
                    self.state = OverlayState::Result(cached.clone());
                    vec![
                        UiEffect::SendCancel,
                        UiEffect::WriteClipboard(cached),
                        UiEffect::ResetAreas,
                    ]
                } else {
                    // Cache miss: cancel current, re-send with new mode.
                    self.next_request_id += 1;
                    self.current_request_id = self.next_request_id;
                    let mut effects = vec![UiEffect::SendCancel];
                    if let Some(text) = self.original_text.clone() {
                        effects.push(UiEffect::SendProcess {
                            text,
                            mode: self.mode,
                            request_id: self.current_request_id,
                        });
                    }
                    effects
                }
            }
            OverlayState::Result(_) | OverlayState::Error(_) => {
                if let Some(cached) = self.mode_cache.get(&new_mode).cloned() {
                    // Cache hit: return cached result directly.
                    self.state = OverlayState::Result(cached.clone());
                    vec![
                        UiEffect::WriteClipboard(cached),
                        UiEffect::ResetAreas,
                    ]
                } else {
                    // Cache miss: re-process with new mode.
                    self.next_request_id += 1;
                    self.current_request_id = self.next_request_id;
                    self.state = OverlayState::Processing;
                    let mut effects = Vec::new();
                    if let Some(text) = self.original_text.clone() {
                        effects.push(UiEffect::SendProcess {
                            text,
                            mode: self.mode,
                            request_id: self.current_request_id,
                        });
                    }
                    effects.push(UiEffect::ResetAreas);
                    effects
                }
            }
            OverlayState::Hidden => vec![],
        }
    }

    fn on_focus_lost(&mut self) -> Vec<UiEffect> {
        if matches!(self.state, OverlayState::Hidden) || !self.has_been_focused {
            return vec![];
        }
        self.state = OverlayState::Hidden;
        self.original_text = None;
        self.mode_cache.clear();
        self.has_been_focused = false;
        vec![UiEffect::HideWindow]
    }

    fn on_clipboard_write_error(&mut self, msg: String) -> Vec<UiEffect> {
        // Must NOT emit WriteClipboard to avoid infinite recursion.
        self.state = OverlayState::Error(msg);
        vec![UiEffect::ResetAreas, UiEffect::ShowWindow]
    }

    fn check_invariants(&self) {
        debug_assert!(
            !matches!(self.state, OverlayState::Processing) || self.original_text.is_some(),
            "invariant violated: Processing state requires original_text"
        );
        debug_assert!(
            !matches!(self.state, OverlayState::Hidden) || self.original_text.is_none(),
            "invariant violated: Hidden state should have no original_text"
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

    /// Helper: feed TextReady and return the effects.
    fn start_processing(sm: &mut StateMachine, text: &str) -> Vec<UiEffect> {
        sm.handle(UiEvent::TextReady(text.to_string()))
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
            request_id: rid,
        });

        assert_eq!(*sm.state(), OverlayState::Result("translated".into()));
        assert!(effects.contains(&UiEffect::WriteClipboard("translated".into())));
        assert!(effects.contains(&UiEffect::ShowWindow));
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
        assert!(effects.contains(&UiEffect::ShowWindow));
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

        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Correct));

        assert_eq!(*sm.state(), OverlayState::Processing);
        assert_eq!(sm.mode(), ProcessMode::Correct);
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
            request_id: rid,
        });

        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Correct));

        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { mode: ProcessMode::Correct, .. })));
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

        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Correct));

        assert_eq!(*sm.state(), OverlayState::Hidden);
        assert_eq!(sm.mode(), ProcessMode::Correct);
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
            request_id: rid1,
        });

        assert!(effects.is_empty());
        assert_eq!(*sm.state(), OverlayState::Processing);

        // Current response works.
        let effects = sm.handle(UiEvent::WorkerResult {
            text: "current".into(),
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

        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Correct));

        assert_eq!(sm.original_text(), Some("hello"));
    }

    // === Focus loss ===

    #[test]
    fn focus_lost_hides_when_focused() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");
        sm.set_focused();

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
        sm.set_focused();

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
            request_id: rid,
        });

        // Now in Result state — cancel should do nothing.
        let effects = sm.handle(UiEvent::UserCancel);

        assert!(effects.is_empty());
        assert_eq!(*sm.state(), OverlayState::Result("ok".into()));
    }

    #[test]
    fn clipboard_write_error_transitions_to_error() {
        let mut sm = new_sm();
        start_processing(&mut sm, "hello");

        let effects = sm.handle(UiEvent::ClipboardWriteError("write failed".into()));

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
            request_id: rid,
        });
        assert!(sm.original_text().is_some());

        // Result -> Processing (mode switch)
        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Correct));
        let rid2 = sm.current_request_id();
        assert!(sm.original_text().is_some());

        // Processing -> Result
        sm.handle(UiEvent::WorkerResult {
            text: "corrected".into(),
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
            request_id: rid,
        });
        // Switch to Correct → Result
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Correct));
        let rid = last_request_id(&effects);
        sm.handle(UiEvent::WorkerResult {
            text: "corrected".into(),
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
            request_id: rid,
        });
        // Switch to Correct → Processing (in-flight)
        sm.handle(UiEvent::UserSwitchMode(ProcessMode::Correct));
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
            request_id: rid,
        });
        // Switch to Correct → Error
        let effects = sm.handle(UiEvent::UserSwitchMode(ProcessMode::Correct));
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
            request_id: rid,
        });
        // Cache now has Translate → "translated"

        // New text arrives → cache cleared, re-processes
        let effects = sm.handle(UiEvent::TextReady("world".into()));
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
            request_id: rid,
        });

        // Close overlay → cache cleared
        sm.handle(UiEvent::UserClose);
        assert_eq!(*sm.state(), OverlayState::Hidden);

        // Re-enter with same text: should go to Processing (not cached)
        let effects = sm.handle(UiEvent::TextReady("hello".into()));
        assert_eq!(*sm.state(), OverlayState::Processing);
        assert!(effects.iter().any(|e| matches!(e, UiEffect::SendProcess { .. })));
    }
}
