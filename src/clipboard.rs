use std::thread;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use tracing::{debug, info, warn};

use crate::platform::Platform;
use crate::ClipboardError;

const CLIPBOARD_POLL_INTERVAL_MS: u64 = 50;
const CLIPBOARD_POLL_TIMEOUT_SECS: u64 = 2;

pub struct ClipboardManager {
    board: Clipboard,
}

impl ClipboardManager {
    pub fn new() -> Result<Self, ClipboardError> {
        let board =
            Clipboard::new().map_err(|e| ClipboardError::AccessFailed(e.to_string()))?;
        Ok(Self { board })
    }

    /// Read text from clipboard. If empty, simulate copy via platform and poll for change.
    pub fn read_text(&mut self, platform: &dyn Platform) -> Result<String, ClipboardError> {
        // Try reading current clipboard content.
        let current = self
            .board
            .get_text()
            .unwrap_or_default();

        if !current.is_empty() {
            info!("clipboard already has text ({} chars)", current.len());
            return Ok(current);
        }

        // Clipboard empty — simulate copy and poll for new content.
        info!("clipboard empty, simulating copy");
        platform.simulate_copy()?;

        let deadline = Instant::now() + Duration::from_secs(CLIPBOARD_POLL_TIMEOUT_SECS);
        let interval = Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS);

        loop {
            thread::sleep(interval);

            let text = self
                .board
                .get_text()
                .unwrap_or_default();

            if !text.is_empty() {
                debug!("clipboard changed after copy simulation ({} chars)", text.len());
                return Ok(text);
            }

            if Instant::now() >= deadline {
                warn!("clipboard poll timed out after {}s", CLIPBOARD_POLL_TIMEOUT_SECS);
                return Err(ClipboardError::NoTextAfterCopy);
            }
        }
    }

    /// Write text to clipboard.
    pub fn write_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        self.board
            .set_text(text)
            .map_err(|e| ClipboardError::WriteFailed(e.to_string()))?;
        info!("wrote {} chars to clipboard", text.len());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PlatformError;
    use std::sync::Mutex;

    // Serialize clipboard tests — they share the system clipboard.
    static CLIPBOARD_LOCK: Mutex<()> = Mutex::new(());

    struct MockPlatform {
        copy_result: Result<(), PlatformError>,
    }

    impl Platform for MockPlatform {
        fn simulate_copy(&self) -> Result<(), PlatformError> {
            self.copy_result
                .as_ref()
                .map(|_| ())
                .map_err(|e| PlatformError::CopyFailed(e.to_string()))
        }

        fn check_accessibility(&self) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    #[test]
    fn write_then_read() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();
        let mock = MockPlatform {
            copy_result: Ok(()),
        };

        mgr.write_text("test clipboard content").unwrap();
        let text = mgr.read_text(&mock).unwrap();
        assert_eq!(text, "test clipboard content");
    }

    #[test]
    fn write_empty_then_fallback_timeout() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();
        let _ = mgr.board.clear();

        let mock = MockPlatform {
            copy_result: Ok(()),
        };

        let result = mgr.read_text(&mock);
        assert!(matches!(result, Err(ClipboardError::NoTextAfterCopy)));
    }

    #[test]
    fn fallback_copy_simulation_fails() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();
        let _ = mgr.board.clear();

        let mock = MockPlatform {
            copy_result: Err(PlatformError::CopyFailed("test error".into())),
        };

        let result = mgr.read_text(&mock);
        assert!(matches!(result, Err(ClipboardError::CopyFailed(_))));
    }

    #[test]
    fn write_overwrites_previous() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();
        let mock = MockPlatform {
            copy_result: Ok(()),
        };

        mgr.write_text("first").unwrap();
        mgr.write_text("second").unwrap();
        let text = mgr.read_text(&mock).unwrap();
        assert_eq!(text, "second");
    }
}
