use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use tracing::{debug, info, warn};

use crate::platform::Platform;
use crate::ClipboardError;

const CLIPBOARD_POLL_INTERVAL_MS: u64 = 50;
const CLIPBOARD_POLL_TIMEOUT_SECS: u64 = 2;

/// Clipboard content: text, images, or both.
#[derive(Debug, Clone)]
pub struct ClipboardContent {
    pub text: Option<String>,
    /// PNG-encoded images. Vec for future multi-image support;
    /// currently arboard provides at most one.
    pub images: Vec<Arc<Vec<u8>>>,
}

impl ClipboardContent {
    /// Create text-only content (no images).
    pub fn text_only(text: String) -> Self {
        Self {
            text: Some(text),
            images: vec![],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_none() && self.images.is_empty()
    }

    pub fn has_images(&self) -> bool {
        !self.images.is_empty()
    }
}

/// Encode raw RGBA pixel data to PNG.
fn rgba_to_png(bytes: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ClipboardError> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| ClipboardError::ImageEncodeFailed(e.to_string()))?;
        writer
            .write_image_data(bytes)
            .map_err(|e| ClipboardError::ImageEncodeFailed(e.to_string()))?;
    }
    Ok(out)
}

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

    /// Read current clipboard content (text + images).
    /// Returns error if clipboard is completely empty.
    pub fn read_content(&mut self) -> Result<ClipboardContent, ClipboardError> {
        let text = self.board.get_text().ok().filter(|s| !s.is_empty());

        let images = match self.board.get_image() {
            Ok(img) => {
                let png = rgba_to_png(
                    img.bytes.as_ref(),
                    img.width as u32,
                    img.height as u32,
                )?;
                info!("read clipboard image ({}x{}, {} bytes PNG)", img.width, img.height, png.len());
                vec![Arc::new(png)]
            }
            Err(_) => vec![],
        };

        let content = ClipboardContent { text, images };
        if content.is_empty() {
            return Err(ClipboardError::NoTextInClipboard);
        }

        if let Some(ref t) = content.text {
            info!("read clipboard text ({} chars)", t.len());
        }
        Ok(content)
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

        fn mouse_position(&self) -> Option<(f64, f64)> {
            None
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

    // -- ClipboardContent unit tests --

    #[test]
    fn clipboard_content_is_empty() {
        let content = ClipboardContent {
            text: None,
            images: vec![],
        };
        assert!(content.is_empty());
        assert!(!content.has_images());
    }

    #[test]
    fn clipboard_content_text_only() {
        let content = ClipboardContent::text_only("hello".into());
        assert!(!content.is_empty());
        assert!(!content.has_images());
        assert_eq!(content.text.as_deref(), Some("hello"));
    }

    #[test]
    fn clipboard_content_image_only() {
        let content = ClipboardContent {
            text: None,
            images: vec![Arc::new(vec![0x89, 0x50, 0x4E, 0x47])],
        };
        assert!(!content.is_empty());
        assert!(content.has_images());
    }

    // -- rgba_to_png tests --

    #[test]
    fn rgba_to_png_valid_data() {
        // 2x2 RGBA pixels (16 bytes)
        let pixels: Vec<u8> = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let png = rgba_to_png(&pixels, 2, 2).unwrap();

        // PNG signature: 0x89 P N G
        assert!(png.len() > 8);
        assert_eq!(&png[..4], &[0x89, 0x50, 0x4E, 0x47]);
    }

    #[test]
    fn rgba_to_png_invalid_dimensions() {
        // 3 bytes is not enough for any valid RGBA image
        let result = rgba_to_png(&[0, 0, 0], 2, 2);
        assert!(matches!(result, Err(ClipboardError::ImageEncodeFailed(_))));
    }
}
