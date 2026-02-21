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

    /// Read current clipboard text directly. Returns error if clipboard is empty.
    pub fn read_clipboard(&mut self) -> Result<String, ClipboardError> {
        let text = self.board.get_text().unwrap_or_default();
        if text.is_empty() {
            return Err(ClipboardError::NoTextInClipboard);
        }
        info!("read clipboard ({} chars)", text.len());
        debug!("clipboard text: {text}");
        Ok(text)
    }

    /// Simulate copy via platform, then poll clipboard for new content.
    /// Clears clipboard first so we can detect when new content arrives.
    pub fn copy_and_read(&mut self, platform: &dyn Platform) -> Result<String, ClipboardError> {
        info!("simulating copy to capture selection");
        // Wait for user to release modifier keys (Ctrl+Shift) after double-tap,
        // otherwise simulate_copy sends Cmd+Ctrl+Shift+C instead of Cmd+C.
        thread::sleep(Duration::from_millis(200));
        let _ = self.board.clear();
        platform.simulate_copy()?;

        let deadline = Instant::now() + Duration::from_secs(CLIPBOARD_POLL_TIMEOUT_SECS);
        let interval = Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS);

        loop {
            thread::sleep(interval);

            let text = self.board.get_text().unwrap_or_default();

            if !text.is_empty() {
                debug!("clipboard has text after copy simulation ({} chars)", text.len());
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
        debug!("written text: {text}");
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
        /// If set, simulate_copy writes this text to clipboard.
        copy_text: Option<String>,
    }

    impl Platform for MockPlatform {
        fn simulate_copy(&self) -> Result<(), PlatformError> {
            self.copy_result
                .as_ref()
                .map_err(|e| PlatformError::CopyFailed(e.to_string()))?;
            if let Some(text) = &self.copy_text {
                let mut board = Clipboard::new().unwrap();
                board.set_text(text).unwrap();
            }
            Ok(())
        }

        fn check_accessibility(&self) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    #[test]
    fn read_clipboard_returns_text() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();
        mgr.write_text("test clipboard content").unwrap();
        let text = mgr.read_clipboard().unwrap();
        assert_eq!(text, "test clipboard content");
    }

    #[test]
    fn read_clipboard_empty_returns_error() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();
        let _ = mgr.board.clear();
        let result = mgr.read_clipboard();
        assert!(matches!(result, Err(ClipboardError::NoTextInClipboard)));
    }

    #[test]
    fn copy_and_read_captures_selection() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();
        let mock = MockPlatform {
            copy_result: Ok(()),
            copy_text: Some("selected text".into()),
        };

        // Pre-existing clipboard content should be replaced by copy simulation.
        mgr.write_text("old content").unwrap();
        let text = mgr.copy_and_read(&mock).unwrap();
        assert_eq!(text, "selected text");
    }

    #[test]
    fn copy_and_read_empty_times_out() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();
        let _ = mgr.board.clear();

        let mock = MockPlatform {
            copy_result: Ok(()),
            copy_text: None,
        };

        let result = mgr.copy_and_read(&mock);
        assert!(matches!(result, Err(ClipboardError::NoTextAfterCopy)));
    }

    #[test]
    fn copy_and_read_simulation_fails() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();
        let _ = mgr.board.clear();

        let mock = MockPlatform {
            copy_result: Err(PlatformError::CopyFailed("test error".into())),
            copy_text: None,
        };

        let result = mgr.copy_and_read(&mock);
        assert!(matches!(result, Err(ClipboardError::CopyFailed(_))));
    }

    #[test]
    fn write_overwrites_previous() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();

        mgr.write_text("first").unwrap();
        mgr.write_text("second").unwrap();
        let text = mgr.read_clipboard().unwrap();
        assert_eq!(text, "second");
    }
}
