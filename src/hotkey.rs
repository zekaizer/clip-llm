use std::time::{Duration, Instant};

use tracing::debug;

/// Default timeout window for double-tap detection, used when the config does
/// not override it (`[hotkey].double_tap_timeout_ms`).
///
/// A single-tap shows nothing until this window expires, so the value is a
/// direct perceived-latency cost on every single-tap. Typical double-tap gaps
/// are 250-350ms; 350ms keeps double-taps reliable while cutting the
/// single-tap dead time from ~550ms to ~400ms (window + 50ms poll tick).
pub const DEFAULT_DOUBLE_TAP_TIMEOUT: Duration = Duration::from_millis(350);

/// Result of a hotkey press event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapAction {
    /// First tap registered, waiting for potential second tap.
    Pending,
    /// Single-tap confirmed after timeout elapsed without a second tap.
    SingleTap,
    /// Double-tap confirmed within the timeout window.
    DoubleTap,
    /// An extra C tap while the modifiers stay held after the trigger — advance
    /// the mode-cycle preview by one step.
    CycleAdvance,
    /// The cycle modifiers (Ctrl+Shift) were released — commit the previewed
    /// mode. `is_double_tap` carries the trigger kind so the UI can run the
    /// deferred selection capture when committing a double-tap gesture.
    CycleCommit { is_double_tap: bool },
}

/// Tap action with mouse position captured at first key press.
#[derive(Debug, Clone, Copy)]
pub struct TapEvent {
    pub action: TapAction,
    /// Mouse position at the moment of the first key press, in platform
    /// screen coordinates (see `Platform::mouse_position`).
    pub mouse_pos: Option<(f64, f64)>,
}

/// Detects single-tap vs double-tap of a hotkey.
///
/// - `on_press()` returns `Pending` on first tap, `DoubleTap` on second tap within timeout.
/// - `check_timeout()` returns `true` when a pending single-tap has expired (single-tap confirmed).
pub struct HotkeyDetector {
    last_press: Option<Instant>,
    /// Double-tap detection window.
    timeout: Duration,
}

impl Default for HotkeyDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl HotkeyDetector {
    /// Create a detector with the default double-tap timeout.
    pub fn new() -> Self {
        Self::with_timeout(DEFAULT_DOUBLE_TAP_TIMEOUT)
    }

    /// Create a detector with a custom double-tap timeout.
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            last_press: None,
            timeout,
        }
    }

    /// Call on each hotkey press event.
    /// Returns `DoubleTap` if this press completes a double-tap within the timeout,
    /// otherwise returns `Pending`.
    pub fn on_press(&mut self) -> TapAction {
        let now = Instant::now();
        if let Some(last) = self.last_press.take()
            && now.duration_since(last) <= self.timeout
        {
            debug!("double-tap detected");
            return TapAction::DoubleTap;
        }
        // First tap or timeout expired — record and wait.
        self.last_press = Some(now);
        debug!("single tap registered, waiting for potential double-tap");
        TapAction::Pending
    }

    /// Whether a first-tap is pending (waiting for potential double-tap).
    pub fn is_pending(&self) -> bool {
        self.last_press.is_some()
    }

    /// Check if a pending single-tap has timed out.
    /// Returns `true` when a first tap was recorded and the timeout has elapsed,
    /// confirming a single-tap action.
    pub fn check_timeout(&mut self) -> bool {
        if let Some(last) = self.last_press
            && last.elapsed() > self.timeout
        {
            self.last_press = None;
            debug!("single-tap confirmed (timeout elapsed)");
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn single_press_returns_pending() {
        let mut d = HotkeyDetector::new();
        assert_eq!(d.on_press(), TapAction::Pending);
    }

    #[test]
    fn double_press_returns_double_tap() {
        let mut d = HotkeyDetector::new();
        assert_eq!(d.on_press(), TapAction::Pending);
        assert_eq!(d.on_press(), TapAction::DoubleTap);
    }

    #[test]
    fn timeout_resets_to_pending() {
        let mut d = HotkeyDetector::new();
        assert_eq!(d.on_press(), TapAction::Pending);
        thread::sleep(Duration::from_millis(550));
        assert_eq!(d.on_press(), TapAction::Pending);
    }

    #[test]
    fn resets_after_double_tap() {
        let mut d = HotkeyDetector::new();
        assert_eq!(d.on_press(), TapAction::Pending);
        assert_eq!(d.on_press(), TapAction::DoubleTap);
        assert_eq!(d.on_press(), TapAction::Pending);
    }

    #[test]
    fn check_timeout_confirms_single_tap() {
        let mut d = HotkeyDetector::new();
        assert_eq!(d.on_press(), TapAction::Pending);
        assert!(!d.check_timeout()); // not yet expired
        thread::sleep(Duration::from_millis(550));
        assert!(d.check_timeout()); // now expired → single-tap confirmed
        assert!(!d.check_timeout()); // consumed, no longer pending
    }

    #[test]
    fn check_timeout_not_triggered_after_double_tap() {
        let mut d = HotkeyDetector::new();
        assert_eq!(d.on_press(), TapAction::Pending);
        assert_eq!(d.on_press(), TapAction::DoubleTap);
        thread::sleep(Duration::from_millis(550));
        assert!(!d.check_timeout()); // double-tap consumed last_press
    }
}
