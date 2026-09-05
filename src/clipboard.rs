use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use tracing::{debug, info, warn};

use crate::platform::{ModifierState, Platform};
use crate::ClipboardError;

const CLIPBOARD_POLL_INTERVAL_MS: u64 = 50;
const CLIPBOARD_POLL_TIMEOUT_MS: u64 = 500;

/// Hard cap on how long `copy_and_read` waits for the user's Ctrl+Shift
/// hotkey modifiers to release before simulating the copy chord. Bounds the
/// wait so a stuck or unavailable watcher can never stall a capture.
const MODIFIER_RELEASE_TIMEOUT_MS: u64 = 300;
/// Poll resolution while waiting for modifier release.
const MODIFIER_RELEASE_POLL_INTERVAL_MS: u64 = 10;
/// Short settle delay applied after modifiers are confirmed released (or the
/// watcher is unavailable), giving the OS event queue a moment to flush
/// before the synthetic copy chord is posted.
const MODIFIER_RELEASE_GRACE_MS: u64 = 40;
/// Fallback flat settle delay used when no live modifier watcher is wired in
/// (e.g. a platform build with no watcher, or one that failed to install).
/// This is the pre-#watcher behavior, kept as a safety net.
const FALLBACK_MODIFIER_SETTLE_MS: u64 = 200;

/// Clipboard content: text, images, or both.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipboardContent {
    pub text: Option<String>,
    /// PNG-encoded images. Vec for future multi-image support;
    /// currently arboard provides at most one.
    pub images: Vec<Arc<Vec<u8>>>,
    /// Display names of the files this content was read from (file-list
    /// clipboard), empty for plain text/image content. Shown in the overlay's
    /// source badge; never sent to the model.
    pub files: Vec<String>,
}

impl ClipboardContent {
    /// Create text-only content (no images).
    pub fn text_only(text: String) -> Self {
        Self {
            text: Some(text),
            images: vec![],
            files: vec![],
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

    /// Whether the content is image-only: no text field at all, but one or more
    /// images. Note this keys on `text.is_none()`, not [`has_text`](Self::has_text)
    /// (which also rejects whitespace-only text) — it mirrors the mode-gating the
    /// UI applies to a captured clipboard.
    pub fn is_image_only(&self) -> bool {
        self.text.is_none() && self.has_images()
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
            let png = encode_rgba_for_upload(img.bytes.into_owned(), img.width as u32, img.height as u32)?;
            Ok(vec![Arc::new(png)])
        }
        Err(_) => Ok(vec![]),
    }
}

/// Downscale (long edge > `MAX_IMAGE_LONG_EDGE`) and PNG-encode raw RGBA pixels
/// for the vision API. Shared by the clipboard image and PNG-file paths so both
/// obey the same payload bound.
pub(crate) fn encode_rgba_for_upload(
    rgba: Vec<u8>,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, ClipboardError> {
    let (bytes, w, h) = match downscale_rgba(&rgba, width, height) {
        Some((resized, new_w, new_h)) => {
            debug!("downscaling image {}x{} -> {}x{}", width, height, new_w, new_h);
            (resized, new_w, new_h)
        }
        None => (rgba, width, height),
    };
    let png = rgba_to_png(&bytes, w, h)?;
    info!("encoded image ({}x{}, {} bytes PNG)", w, h, png.len());
    Ok(png)
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
pub(crate) fn rgba_to_png(bytes: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ClipboardError> {
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

/// Wait for the user's Ctrl+Shift hotkey modifiers to release before a copy
/// simulation is posted, so the synthetic chord isn't contaminated by keys
/// the user is still physically holding down. Polls `is_held` at
/// `MODIFIER_RELEASE_POLL_INTERVAL_MS` resolution up to a
/// `MODIFIER_RELEASE_TIMEOUT_MS` hard cap — bounding the wait so a
/// stuck/unavailable watcher can never stall a capture — then applies a short
/// grace sleep for OS event-queue settling. `cancel` is checked on every poll
/// tick (and once more after the loop) so an aborted capture returns promptly
/// instead of riding out the full wait.
///
/// `is_held` is injected (rather than taking `&ModifierState` directly) so
/// this is unit-testable without a real OS hook.
fn wait_for_modifier_release(
    is_held: impl Fn() -> bool,
    cancel: &AtomicBool,
) -> Result<(), ClipboardError> {
    let deadline = Instant::now() + Duration::from_millis(MODIFIER_RELEASE_TIMEOUT_MS);
    while is_held() {
        if cancel.load(Ordering::SeqCst) {
            debug!("capture cancelled while waiting for modifier release");
            return Err(ClipboardError::Cancelled);
        }
        if Instant::now() >= deadline {
            debug!("modifier release wait timed out after {MODIFIER_RELEASE_TIMEOUT_MS}ms");
            break;
        }
        thread::sleep(Duration::from_millis(MODIFIER_RELEASE_POLL_INTERVAL_MS));
    }
    if cancel.load(Ordering::SeqCst) {
        debug!("capture cancelled before copy simulation");
        return Err(ClipboardError::Cancelled);
    }
    thread::sleep(Duration::from_millis(MODIFIER_RELEASE_GRACE_MS));
    Ok(())
}

pub struct ClipboardManager {
    board: Clipboard,
    /// Live Ctrl+Shift hold state, consulted by `copy_and_read` to wait for an
    /// actual modifier release instead of a flat settle delay. `None` when no
    /// watcher handle has been attached (e.g. callers that never simulate a
    /// copy) — `copy_and_read` falls back to the pre-watcher fixed delay in
    /// that case, and also whenever the attached watcher never came up
    /// (`ModifierState::is_active() == false`).
    modifier_state: Option<ModifierState>,
}

impl ClipboardManager {
    pub fn new() -> Result<Self, ClipboardError> {
        let board =
            Clipboard::new().map_err(|e| ClipboardError::AccessFailed(e.to_string()))?;
        Ok(Self { board, modifier_state: None })
    }

    /// Attach a live modifier-state handle so `copy_and_read` can wait for an
    /// actual Ctrl+Shift release instead of sleeping a flat delay. Wire this
    /// in wherever the manager used for selection capture is constructed
    /// (currently `ui::OverlayApp::start_capture`, sourced from the
    /// process-lifetime watcher spawned in `main.rs`).
    pub fn with_modifier_state(mut self, modifier_state: ModifierState) -> Self {
        self.modifier_state = Some(modifier_state);
        self
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
        // Wait for the user to release the hotkey modifiers (Ctrl+Shift) before
        // simulating the copy, otherwise simulate_copy sends Cmd+Ctrl+Shift+C
        // instead of Cmd+C. With a live watcher this is usually near-instant:
        // capture only reaches here via CycleCommit, which the coordinator
        // already gates on an observed release (see coordinator::resolve_trigger),
        // so most of the time modifiers are already up and only the short grace
        // sleep applies. The bounded poll below is a safety net for the rare
        // case they are still coming up, and the flat fallback below covers
        // platform builds with no watcher.
        match self.modifier_state.as_ref().filter(|m| m.is_active()) {
            Some(state) => wait_for_modifier_release(|| state.combo_held(), cancel)?,
            None => {
                thread::sleep(Duration::from_millis(FALLBACK_MODIFIER_SETTLE_MS));
                if cancel.load(Ordering::SeqCst) {
                    debug!("capture cancelled before copy simulation");
                    return Err(ClipboardError::Cancelled);
                }
            }
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
                // Files first — see read_content.
                if let Some(paths) = self.file_list() {
                    return crate::files::ingest_files(&paths);
                }
                let text = self.board.get_text().ok().filter(|s| !s.trim().is_empty());
                let images = read_image_from_board(&mut self.board)?;

                if text.is_some() || !images.is_empty() {
                    let content = ClipboardContent { text, images, files: vec![] };
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
        // Files first: Finder/Explorer also publish a plain-text flavor holding
        // just the file names, which a text read would mistake for content.
        if let Some(paths) = self.file_list() {
            return crate::files::ingest_files(&paths);
        }
        let text = self.board.get_text().ok().filter(|s| !s.trim().is_empty());

        let images = read_image_from_board(&mut self.board)?;

        let content = ClipboardContent { text, images, files: vec![] };
        if content.is_empty() {
            return Err(ClipboardError::NoTextInClipboard);
        }

        if let Some(ref t) = content.text {
            info!("read clipboard text ({} chars)", t.len());
        }
        Ok(content)
    }

    /// Paths of a file-list clipboard (Finder/Explorer copy), if any.
    fn file_list(&mut self) -> Option<Vec<std::path::PathBuf>> {
        self.board.get().file_list().ok().filter(|v| !v.is_empty())
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
    use std::sync::{Mutex, MutexGuard, PoisonError};

    static CLIPBOARD_LOCK: Mutex<()> = Mutex::new(());

    /// Acquire the clipboard-serialization lock, recovering from poisoning.
    /// The guarded data is `()`, so a panic while holding it corrupts nothing;
    /// without the recovery, one failing clipboard test poisons the lock and
    /// every later test that takes it dies with a PoisonError, burying the
    /// real failure under a dozen cascaded ones.
    pub(crate) fn lock_clipboard() -> MutexGuard<'static, ()> {
        CLIPBOARD_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::lock_clipboard;
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
        fn launch_at_login_enabled(&self) -> bool { false }
        fn set_launch_at_login(&self, _enabled: bool) -> Result<(), PlatformError> { Ok(()) }
    }

    #[test]
    fn read_content_prefers_file_list_over_text_flavor() {
        let _lock = lock_clipboard();
        let dir = std::env::temp_dir().join(format!("clip-llm-cb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("from-finder.txt");
        std::fs::write(&path, "file body\n").unwrap();

        let mut mgr = ClipboardManager::new().unwrap();
        mgr.board.set().file_list(std::slice::from_ref(&path)).unwrap();
        let content = mgr.read_content().unwrap();
        assert_eq!(content.text.as_deref(), Some("file body\n"));
        assert_eq!(content.files, vec!["from-finder.txt".to_string()]);

        // Plain text afterwards must not be mistaken for a stale file list.
        mgr.write_text("plain").unwrap();
        let content = mgr.read_content().unwrap();
        assert_eq!(content.text.as_deref(), Some("plain"));
        assert!(content.files.is_empty());
    }

    #[test]
    fn read_clipboard_returns_text() {
        let _lock = lock_clipboard();
        let mut mgr = ClipboardManager::new().unwrap();
        mgr.write_text("test clipboard content").unwrap();
        let text = mgr.read_clipboard().unwrap();
        assert_eq!(text, "test clipboard content");
    }

    #[test]
    fn read_clipboard_empty_returns_error() {
        let _lock = lock_clipboard();
        let mut mgr = ClipboardManager::new().unwrap();
        let _ = mgr.board.clear();
        let result = mgr.read_clipboard();
        assert!(matches!(result, Err(ClipboardError::NoTextInClipboard)));
    }

    #[test]
    fn read_clipboard_whitespace_only_returns_error() {
        let _lock = lock_clipboard();
        let mut mgr = ClipboardManager::new().unwrap();
        mgr.write_text("   \n\t  ").unwrap();
        let result = mgr.read_clipboard();
        assert!(matches!(result, Err(ClipboardError::NoTextInClipboard)));
    }

    #[test]
    fn copy_and_read_whitespace_only_reports_empty_copy() {
        let _lock = lock_clipboard();
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
        let _lock = lock_clipboard();
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
        let _lock = lock_clipboard();
        let mut mgr = ClipboardManager::new().unwrap();
        mgr.write_text("   ").unwrap();
        // No image in clipboard either → empty content → error.
        let result = mgr.read_content();
        assert!(matches!(result, Err(ClipboardError::NoTextInClipboard)));
    }

    #[test]
    fn copy_and_read_captures_selection() {
        let _lock = lock_clipboard();
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
        let _lock = lock_clipboard();
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
        let _lock = lock_clipboard();
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
        let _lock = lock_clipboard();
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
            files: vec![],
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
            files: vec![],
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
        let content = ClipboardContent { text: None, images: vec![], files: vec![] };
        assert!(!content.has_text());
    }

    #[test]
    fn has_text_whitespace_only() {
        let content = ClipboardContent { text: Some("  \n ".into()), images: vec![], files: vec![] };
        assert!(!content.has_text());
    }

    #[test]
    fn has_text_empty_string() {
        let content = ClipboardContent { text: Some("".into()), images: vec![], files: vec![] };
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

    // -- wait_for_modifier_release tests --

    #[test]
    fn wait_for_modifier_release_fast_path_when_not_held() {
        // Modifiers already released: no poll loop, just the grace sleep.
        let cancel = AtomicBool::new(false);
        let start = Instant::now();
        let result = wait_for_modifier_release(|| false, &cancel);
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        assert!(elapsed < Duration::from_millis(MODIFIER_RELEASE_TIMEOUT_MS));
    }

    #[test]
    fn wait_for_modifier_release_waits_until_released() {
        // Reports held for the first couple of polls, then released.
        let calls = std::cell::Cell::new(0u32);
        let is_held = || {
            let c = calls.get();
            calls.set(c + 1);
            c < 2
        };
        let cancel = AtomicBool::new(false);
        let start = Instant::now();
        let result = wait_for_modifier_release(is_held, &cancel);
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        // At least 2 poll intervals elapsed, but well short of the hard timeout.
        assert!(elapsed >= Duration::from_millis(MODIFIER_RELEASE_POLL_INTERVAL_MS * 2));
        assert!(elapsed < Duration::from_millis(MODIFIER_RELEASE_TIMEOUT_MS));
    }

    #[test]
    fn wait_for_modifier_release_times_out_when_stuck_held() {
        // Never releases: the hard timeout must still bound the wait.
        let cancel = AtomicBool::new(false);
        let start = Instant::now();
        let result = wait_for_modifier_release(|| true, &cancel);
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        assert!(elapsed >= Duration::from_millis(MODIFIER_RELEASE_TIMEOUT_MS));
        // Bounded: timeout + one poll interval slack + the grace sleep, not
        // indefinitely longer.
        assert!(
            elapsed
                < Duration::from_millis(
                    MODIFIER_RELEASE_TIMEOUT_MS
                        + MODIFIER_RELEASE_POLL_INTERVAL_MS
                        + MODIFIER_RELEASE_GRACE_MS
                        + 200
                )
        );
    }

    #[test]
    fn wait_for_modifier_release_cancelled_during_poll_returns_promptly() {
        // Stuck held, but cancelled immediately: must not ride out the full
        // timeout — this is the cancellation-responsiveness guarantee.
        let cancel = AtomicBool::new(true);
        let start = Instant::now();
        let result = wait_for_modifier_release(|| true, &cancel);
        let elapsed = start.elapsed();

        assert!(matches!(result, Err(ClipboardError::Cancelled)));
        assert!(elapsed < Duration::from_millis(MODIFIER_RELEASE_TIMEOUT_MS));
    }

    #[test]
    fn wait_for_modifier_release_cancelled_after_release_before_grace() {
        // Released immediately, but cancelled: the post-loop cancel check
        // must still catch it before the grace sleep.
        let cancel = AtomicBool::new(true);
        let result = wait_for_modifier_release(|| false, &cancel);
        assert!(matches!(result, Err(ClipboardError::Cancelled)));
    }

    // -- copy_and_read modifier-wait integration tests --

    #[test]
    fn copy_and_read_fast_path_when_modifiers_already_released() {
        let _lock = lock_clipboard();
        let mgr = ClipboardManager::new().unwrap();
        let mock = MockPlatform {
            copy_result: Ok(()),
            copy_text: Some("fast selection".into()),
        };

        let modifier_state = ModifierState::default();
        modifier_state.set_active_for_test(true);
        modifier_state.set_combo_held(false);
        let mut mgr = mgr.with_modifier_state(modifier_state);

        let start = Instant::now();
        let content = mgr.copy_and_read(&mock, &AtomicBool::new(false), None).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(content.text.as_deref(), Some("fast selection"));
        // Fast path: grace sleep + one clipboard poll tick, well under the
        // old flat 200ms settle delay this replaces. Wall-clock bounds are
        // unreliable on shared CI runners (observed 393ms under load), so
        // assert the timing only in local runs.
        if std::env::var_os("CI").is_none() {
            assert!(elapsed < Duration::from_millis(150), "elapsed = {elapsed:?}");
        }
    }

    #[test]
    fn copy_and_read_falls_back_to_flat_delay_when_watcher_inactive() {
        let _lock = lock_clipboard();
        let mgr = ClipboardManager::new().unwrap();
        let mock = MockPlatform {
            copy_result: Ok(()),
            copy_text: Some("selection".into()),
        };

        // Attached but never marked active (simulates a watcher that failed
        // to install, or a build with no watcher at all).
        let modifier_state = ModifierState::default();
        let mut mgr = mgr.with_modifier_state(modifier_state);

        let content = mgr.copy_and_read(&mock, &AtomicBool::new(false), None).unwrap();
        assert_eq!(content.text.as_deref(), Some("selection"));
    }
}
