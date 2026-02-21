use std::time::{Duration, Instant};

use tracing::debug;

/// Timeout window for double-tap detection.
const DOUBLE_TAP_TIMEOUT: Duration = Duration::from_millis(500);

/// Detects double-tap of a hotkey within a timeout window.
/// Returns `true` from `on_press()` when the second tap is detected.
pub struct DoubleTapDetector {
    last_press: Option<Instant>,
}

impl Default for DoubleTapDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl DoubleTapDetector {
    pub fn new() -> Self {
        Self { last_press: None }
    }

    /// Call on each hotkey press event. Returns `true` if this press
    /// completes a double-tap within the timeout window.
    pub fn on_press(&mut self) -> bool {
        let now = Instant::now();
        if let Some(last) = self.last_press.take() {
            if now.duration_since(last) <= DOUBLE_TAP_TIMEOUT {
                debug!("double-tap detected");
                return true;
            }
        }
        // First tap or timeout expired — record and wait.
        self.last_press = Some(now);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn single_press_does_not_trigger() {
        let mut d = DoubleTapDetector::new();
        assert!(!d.on_press());
    }

    #[test]
    fn double_press_triggers() {
        let mut d = DoubleTapDetector::new();
        assert!(!d.on_press());
        assert!(d.on_press());
    }

    #[test]
    fn timeout_resets() {
        let mut d = DoubleTapDetector::new();
        assert!(!d.on_press());
        thread::sleep(Duration::from_millis(550));
        assert!(!d.on_press());
    }

    #[test]
    fn resets_after_trigger() {
        let mut d = DoubleTapDetector::new();
        assert!(!d.on_press()); // first tap
        assert!(d.on_press());  // second tap → trigger
        assert!(!d.on_press()); // third tap → new first tap
    }
}
