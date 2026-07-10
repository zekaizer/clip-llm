use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use tracing::{debug, info, warn};

use crate::platform::Platform;
use crate::ClipboardError;

const CLIPBOARD_POLL_INTERVAL_MS: u64 = 50;
const CLIPBOARD_POLL_TIMEOUT_MS: u64 = 500;

/// Clipboard content: text, images, or both.
#[derive(Debug, Clone, PartialEq)]
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

    pub fn has_text(&self) -> bool {
        self.text.as_ref().is_some_and(|t| !t.trim().is_empty())
    }

    pub fn has_images(&self) -> bool {
        !self.images.is_empty()
    }
}

/// Long edge (px) above which a clipboard image is downscaled before PNG
/// encoding. 1568px is a common vision-model sweet spot (e.g. Anthropic/OpenAI
/// vision APIs internally cap around this size); sending anything larger only
/// inflates the base64 payload without improving model accuracy.
const MAX_IMAGE_LONG_EDGE: u32 = 1568;

/// Read the current image from the clipboard and encode it as PNG.
/// Returns an empty vec if no image is present; propagates encoding errors.
/// Oversized images (long edge > `MAX_IMAGE_LONG_EDGE`) are downscaled first to
/// keep the base64-encoded payload small (latency + provider 413 risk).
fn read_image_from_board(board: &mut Clipboard) -> Result<Vec<Arc<Vec<u8>>>, ClipboardError> {
    match board.get_image() {
        Ok(img) => {
            let orig_width = img.width as u32;
            let orig_height = img.height as u32;
            let (bytes, width, height) =
                match downscale_rgba(img.bytes.as_ref(), orig_width, orig_height) {
                    Some((resized, new_w, new_h)) => {
                        debug!(
                            "downscaling clipboard image {}x{} -> {}x{}",
                            orig_width, orig_height, new_w, new_h
                        );
                        (resized, new_w, new_h)
                    }
                    None => (img.bytes.into_owned(), orig_width, orig_height),
                };
            let png = rgba_to_png(&bytes, width, height)?;
            info!(
                "read clipboard image ({}x{}, {} bytes PNG)",
                width,
                height,
                png.len()
            );
            Ok(vec![Arc::new(png)])
        }
        Err(_) => Ok(vec![]),
    }
}

/// Downscale an RGBA image so its long edge is at most `MAX_IMAGE_LONG_EDGE`,
/// preserving aspect ratio. Returns `None` if the image is already within
/// bounds (no-op, avoids copying the buffer). Uses a hand-rolled box (area
/// average) filter rather than pulling in an image-processing crate, per the
/// project's single-binary / minimal-deps constraint.
fn downscale_rgba(bytes: &[u8], width: u32, height: u32) -> Option<(Vec<u8>, u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    let long_edge = width.max(height);
    if long_edge <= MAX_IMAGE_LONG_EDGE {
        return None;
    }

    let scale = MAX_IMAGE_LONG_EDGE as f64 / long_edge as f64;
    let new_width = ((width as f64 * scale).round() as u32).max(1);
    let new_height = ((height as f64 * scale).round() as u32).max(1);

    Some((
        box_resample(bytes, width, height, new_width, new_height),
        new_width,
        new_height,
    ))
}

/// Box-filter (area average) resample of an RGBA buffer from `src_w x src_h`
/// down to `dst_w x dst_h`. Each destination pixel is the average of the
/// source pixels its area covers, which avoids the aliasing a naive
/// nearest-neighbor downscale would introduce.
fn box_resample(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    let x_ratio = src_w as f64 / dst_w as f64;
    let y_ratio = src_h as f64 / dst_h as f64;

    for dy in 0..dst_h {
        let sy0 = (dy as f64 * y_ratio).floor() as u32;
        let sy1 = (((dy + 1) as f64 * y_ratio).ceil() as u32)
            .min(src_h)
            .max(sy0 + 1);
        for dx in 0..dst_w {
            let sx0 = (dx as f64 * x_ratio).floor() as u32;
            let sx1 = (((dx + 1) as f64 * x_ratio).ceil() as u32)
                .min(src_w)
                .max(sx0 + 1);

            let mut r_sum: u64 = 0;
            let mut g_sum: u64 = 0;
            let mut b_sum: u64 = 0;
            let mut a_sum: u64 = 0;
            let mut count: u64 = 0;

            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let idx = ((sy * src_w + sx) * 4) as usize;
                    r_sum += src[idx] as u64;
                    g_sum += src[idx + 1] as u64;
                    b_sum += src[idx + 2] as u64;
                    a_sum += src[idx + 3] as u64;
                    count += 1;
                }
            }

            let didx = ((dy * dst_w + dx) * 4) as usize;
            dst[didx] = (r_sum / count) as u8;
            dst[didx + 1] = (g_sum / count) as u8;
            dst[didx + 2] = (b_sum / count) as u8;
            dst[didx + 3] = (a_sum / count) as u8;
        }
    }

    dst
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
        if text.trim().is_empty() {
            return Err(ClipboardError::NoTextInClipboard);
        }
        info!("read clipboard ({} chars)", text.len());
        debug!("clipboard text: {text}");
        Ok(text)
    }

    /// Simulate copy via platform, then poll the clipboard change counter
    /// until the copy lands. The clipboard is never cleared, so a failed
    /// capture leaves the user's existing content untouched (#48, #57).
    /// `target` is the frontmost-app pid recorded at trigger time, keeping the
    /// simulated copy aimed at the source app even if focus moved since (#55).
    /// Returns both text and images captured by the copy simulation.
    pub fn copy_and_read(
        &mut self,
        platform: &dyn Platform,
        cancel: &AtomicBool,
        target: Option<i32>,
    ) -> Result<ClipboardContent, ClipboardError> {
        info!("simulating copy to capture selection");
        // Wait for user to release modifier keys (Ctrl+Shift) after double-tap,
        // otherwise simulate_copy sends Cmd+Ctrl+Shift+C instead of Cmd+C.
        thread::sleep(Duration::from_millis(200));
        // Bail out before simulating if this capture was cancelled or superseded
        // during the release wait — otherwise Cmd+C would overwrite whatever the
        // user has copied since with stale selection.
        if cancel.load(Ordering::SeqCst) {
            debug!("capture cancelled before copy simulation");
            return Err(ClipboardError::Cancelled);
        }
        let baseline = platform.clipboard_change_count();
        platform.simulate_copy(target)?;

        let start = Instant::now();
        let deadline = start + Duration::from_millis(CLIPBOARD_POLL_TIMEOUT_MS);
        let interval = Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS);
        let mut copy_landed = false;

        loop {
            thread::sleep(interval);

            // Stop polling promptly once superseded so overlapping captures don't
            // keep issuing clipboard reads in parallel.
            if cancel.load(Ordering::SeqCst) {
                debug!("capture cancelled during clipboard poll");
                return Err(ClipboardError::Cancelled);
            }

            // The counter bumps when the source app takes clipboard ownership,
            // which can precede the actual data write (e.g. Windows
            // EmptyClipboard before SetClipboardData) — so keep polling for
            // content until the deadline instead of failing on an empty read.
            if platform.clipboard_change_count() != baseline {
                copy_landed = true;
                let text = self.board.get_text().ok().filter(|s| !s.trim().is_empty());
                let images = read_image_from_board(&mut self.board)?;

                if text.is_some() || !images.is_empty() {
                    let content = ClipboardContent { text, images };
                    let elapsed = start.elapsed().as_millis();
                    info!(
                        "clipboard content arrived in {}ms (text={}, images={})",
                        elapsed,
                        content.text.as_ref().map_or(0, |t| t.len()),
                        content.images.len()
                    );
                    return Ok(content);
                }
            }

            if Instant::now() >= deadline {
                let elapsed = start.elapsed().as_millis();
                if copy_landed {
                    warn!("copy landed but carried no usable content ({elapsed}ms)");
                    return Err(ClipboardError::EmptyCopy);
                }
                warn!("no copy detected within {elapsed}ms");
                return Err(ClipboardError::NoTextAfterCopy);
            }
        }
    }

    /// Read current clipboard content (text + images).
    /// Returns error if clipboard is completely empty.
    pub fn read_content(&mut self) -> Result<ClipboardContent, ClipboardError> {
        let text = self.board.get_text().ok().filter(|s| !s.trim().is_empty());

        let images = read_image_from_board(&mut self.board)?;

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

/// Serializes tests that touch the real system clipboard, since it's shared
/// process-wide state. `pub(crate)` so other modules' tests that also open a
/// real `ClipboardManager` (e.g. `ui::tests`) can serialize against these
/// instead of racing them.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    pub(crate) static CLIPBOARD_LOCK: Mutex<()> = Mutex::new(());
}

#[cfg(test)]
mod tests {
    use super::test_support::CLIPBOARD_LOCK;
    use super::*;
    use crate::PlatformError;

    struct MockPlatform {
        copy_result: Result<(), PlatformError>,
        /// If set, simulate_copy writes this text to clipboard.
        copy_text: Option<String>,
    }

    impl Platform for MockPlatform {
        fn simulate_copy(&self, _target: Option<i32>) -> Result<(), PlatformError> {
            self.copy_result
                .as_ref()
                .map_err(|e| PlatformError::CopyFailed(e.to_string()))?;
            if let Some(text) = &self.copy_text {
                let mut board = Clipboard::new().unwrap();
                board.set_text(text).unwrap();
            }
            Ok(())
        }

        // The mock writes through the real system clipboard, so delegate to
        // the real platform counter to observe those writes.
        fn clipboard_change_count(&self) -> u64 {
            crate::platform::NativePlatform.clipboard_change_count()
        }

        fn frontmost_app_pid(&self) -> Option<i32> {
            None
        }

        fn check_accessibility(&self) -> Result<(), PlatformError> {
            Ok(())
        }

        fn mouse_position(&self) -> Option<(f64, f64)> {
            None
        }

        fn display_bounds_at_point(&self, _x: f64, _y: f64) -> Option<(f64, f64, f64, f64)> {
            None
        }

        fn show_window(&self, _pos: Option<(f32, f32)>) -> bool { false }
        fn show_window_no_activate(&self, _pos: Option<(f32, f32)>) -> bool { false }
        fn hide_window(&self) -> bool { false }
        fn reposition_window(&self, _x: f32, _y: f32) -> bool { false }
        fn paste_to_foreground(&self) -> Result<(), PlatformError> { Ok(()) }
        fn exclude_from_taskbar(&self) {}
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
    fn read_clipboard_whitespace_only_returns_error() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();
        mgr.write_text("   \n\t  ").unwrap();
        let result = mgr.read_clipboard();
        assert!(matches!(result, Err(ClipboardError::NoTextInClipboard)));
    }

    #[test]
    fn copy_and_read_whitespace_only_reports_empty_copy() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();
        let mock = MockPlatform {
            copy_result: Ok(()),
            copy_text: Some("  \n  ".into()),
        };

        // The copy lands (counter bumps) but carries only whitespace.
        let result = mgr.copy_and_read(&mock, &AtomicBool::new(false), None);
        assert!(matches!(result, Err(ClipboardError::EmptyCopy)));
    }

    #[test]
    fn copy_and_read_failure_preserves_clipboard() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();
        let mock = MockPlatform {
            copy_result: Ok(()),
            copy_text: None, // nothing selected — no copy ever lands
        };

        // Regression test for #48: a failed capture must not destroy the
        // user's existing clipboard content (no clear-before-copy).
        mgr.write_text("precious content").unwrap();
        let result = mgr.copy_and_read(&mock, &AtomicBool::new(false), None);
        assert!(matches!(result, Err(ClipboardError::NoTextAfterCopy)));
        assert_eq!(mgr.read_clipboard().unwrap(), "precious content");
    }

    #[test]
    fn read_content_whitespace_only_text_treated_as_no_text() {
        let _lock = CLIPBOARD_LOCK.lock().unwrap();
        let mut mgr = ClipboardManager::new().unwrap();
        mgr.write_text("   ").unwrap();
        // No image in clipboard either → empty content → error.
        let result = mgr.read_content();
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
        let content = mgr.copy_and_read(&mock, &AtomicBool::new(false), None).unwrap();
        assert_eq!(content.text.as_deref(), Some("selected text"));
        // Text-only selection: no images captured.
        assert!(!content.has_images());
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

        let result = mgr.copy_and_read(&mock, &AtomicBool::new(false), None);
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

        let result = mgr.copy_and_read(&mock, &AtomicBool::new(false), None);
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

    // -- has_text tests --

    #[test]
    fn has_text_with_content() {
        let content = ClipboardContent::text_only("hello".into());
        assert!(content.has_text());
    }

    #[test]
    fn has_text_none() {
        let content = ClipboardContent { text: None, images: vec![] };
        assert!(!content.has_text());
    }

    #[test]
    fn has_text_whitespace_only() {
        let content = ClipboardContent { text: Some("  \n ".into()), images: vec![] };
        assert!(!content.has_text());
    }

    #[test]
    fn has_text_empty_string() {
        let content = ClipboardContent { text: Some("".into()), images: vec![] };
        assert!(!content.has_text());
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

    // -- downscale_rgba tests --

    /// Build a solid-color RGBA buffer for testing (cheap, no need for varied pixels).
    fn solid_rgba(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        let mut buf = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            buf.extend_from_slice(&color);
        }
        buf
    }

    #[test]
    fn downscale_rgba_noop_for_small_image() {
        let pixels = solid_rgba(100, 50, [10, 20, 30, 255]);
        let result = downscale_rgba(&pixels, 100, 50);
        assert!(result.is_none());
    }

    #[test]
    fn downscale_rgba_noop_at_exact_threshold() {
        // Long edge exactly at the cap must not be resampled.
        let pixels = solid_rgba(MAX_IMAGE_LONG_EDGE, 100, [1, 2, 3, 255]);
        let result = downscale_rgba(&pixels, MAX_IMAGE_LONG_EDGE, 100);
        assert!(result.is_none());
    }

    #[test]
    fn downscale_rgba_scales_down_preserving_aspect_ratio() {
        // 2x oversized square-ish image: long edge should land exactly on the cap.
        let width = MAX_IMAGE_LONG_EDGE * 2;
        let height = MAX_IMAGE_LONG_EDGE;
        let pixels = solid_rgba(width, height, [200, 100, 50, 255]);

        let (resized, new_w, new_h) = downscale_rgba(&pixels, width, height).unwrap();

        assert_eq!(new_w, MAX_IMAGE_LONG_EDGE);
        assert_eq!(new_h, MAX_IMAGE_LONG_EDGE / 2);
        assert_eq!(resized.len(), (new_w * new_h * 4) as usize);

        // Aspect ratio preserved (within rounding).
        let orig_ratio = width as f64 / height as f64;
        let new_ratio = new_w as f64 / new_h as f64;
        assert!((orig_ratio - new_ratio).abs() < 0.01);
    }

    #[test]
    fn downscale_rgba_solid_color_preserved() {
        // A solid-color image should downscale to the same solid color (box
        // filter averaging a uniform region yields that same value).
        let width = MAX_IMAGE_LONG_EDGE * 2;
        let height = MAX_IMAGE_LONG_EDGE;
        let color = [77, 88, 99, 255];
        let pixels = solid_rgba(width, height, color);

        let (resized, new_w, new_h) = downscale_rgba(&pixels, width, height).unwrap();

        for chunk in resized.chunks_exact(4) {
            assert_eq!(chunk, color);
        }
        assert_eq!(resized.len(), (new_w * new_h * 4) as usize);
    }

    #[test]
    fn downscale_rgba_degenerate_1px_wide() {
        // Extremely tall, 1px-wide image: width must stay clamped to at least 1
        // after scaling, height clamped to the cap.
        let width = 1;
        let height = MAX_IMAGE_LONG_EDGE * 10;
        let pixels = solid_rgba(width, height, [5, 6, 7, 255]);

        let (resized, new_w, new_h) = downscale_rgba(&pixels, width, height).unwrap();

        assert_eq!(new_w, 1);
        assert_eq!(new_h, MAX_IMAGE_LONG_EDGE);
        assert_eq!(resized.len(), (new_w * new_h * 4) as usize);
    }

    #[test]
    fn downscale_rgba_zero_dimension_is_noop() {
        // Degenerate zero-sized image must not panic or divide by zero.
        assert!(downscale_rgba(&[], 0, 0).is_none());
        assert!(downscale_rgba(&[], 0, 100).is_none());
    }
}
