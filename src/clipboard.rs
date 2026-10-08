use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use arboard::Clipboard;
use tracing::{debug, info, warn};

use crate::images::encode::{check_pixel_budget, encode_for_upload};
use crate::images::fetch::fetch_source;
use crate::images::markup::images_from_html;
use crate::images::{ImageAttachment, ImageOrigin};
use crate::platform::{ModifierState, Platform};
use crate::ClipboardError;

/// Resolution of the change-counter poll while a simulated copy is pending.
const CLIPBOARD_POLL_INTERVAL_MS: u64 = 2;
const CLIPBOARD_POLL_TIMEOUT_MS: u64 = 500;
/// Quiet period after the last counter change before the content is read -
/// sources write in several steps (Firefox: two sessions 5-28 ms apart).
const CLIPBOARD_SETTLE_MS: u64 = 30;
/// Re-read interval after a landed copy read back empty - the counter can
/// bump before the data is readable and need not bump again.
const CLIPBOARD_EMPTY_RETRY_MS: u64 = 50;

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
    /// Images ready for upload. The image flavor yields at most one; the
    /// file list and markup paths can yield several.
    pub images: Vec<ImageAttachment>,
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

/// Read the current image from the clipboard and encode it as PNG.
/// Returns an empty vec if no image is present; propagates encoding errors.
/// Oversized images (long edge > `MAX_IMAGE_LONG_EDGE`) are downscaled first to
/// keep the base64-encoded payload small (latency + provider 413 risk).
fn read_image_from_board(board: &mut Clipboard) -> Result<Vec<ImageAttachment>, ClipboardError> {
    match board.get_image() {
        Ok(img) => {
            let (width, height) = (img.width as u32, img.height as u32);
            check_pixel_budget(width, height)?;
            let att = encode_for_upload(img.bytes.into_owned(), width, height, ImageOrigin::Clipboard)?;
            Ok(vec![att])
        }
        Err(_) => Ok(vec![]),
    }
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

/// Run a capture body on its thread so a panic inside it becomes an error
/// the UI receives, instead of a silently dead thread and an overlay stuck in
/// Capturing. The panic is logged at error level with its message.
pub fn guard_capture<F>(body: F) -> Result<ClipboardContent, ClipboardError>
where
    F: FnOnce() -> Result<ClipboardContent, ClipboardError>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "non-string panic payload".to_owned());
            tracing::error!("capture thread panicked: {message}");
            Err(ClipboardError::AccessFailed(format!("capture thread panicked: {message}")))
        }
    }
}

/// Next action for the loop waiting on a simulated copy.
#[derive(Debug, PartialEq, Eq)]
enum WatchStep {
    Wait,
    /// Read the clipboard now; keep polling if it comes back empty.
    Read,
    TimedOut,
}

/// Decides when a simulated copy is ready to read, from change-counter
/// samples alone. The caller supplies the counter and the clock.
struct CopyWatch {
    last_count: u64,
    deadline: Instant,
    /// When the counter last moved; `None` until a copy lands.
    changed_at: Option<Instant>,
    /// When `Read` was last returned for the current counter value.
    read_at: Option<Instant>,
    /// The one read allowed at the deadline has been issued.
    final_read: bool,
}

impl CopyWatch {
    fn new(baseline: u64, now: Instant) -> Self {
        Self {
            last_count: baseline,
            deadline: now + Duration::from_millis(CLIPBOARD_POLL_TIMEOUT_MS),
            changed_at: None,
            read_at: None,
            final_read: false,
        }
    }

    /// Whether the counter has moved off the baseline.
    fn landed(&self) -> bool {
        self.changed_at.is_some()
    }

    fn poll(&mut self, count: u64, now: Instant) -> WatchStep {
        if count != self.last_count {
            self.last_count = count;
            self.changed_at = Some(now);
            self.read_at = None;
        }
        if now >= self.deadline {
            // Bounded: a counter that keeps moving gets one read, not a loop.
            if self.changed_at.is_some() && self.read_at.is_none() && !self.final_read {
                self.final_read = true;
                self.read_at = Some(now);
                return WatchStep::Read;
            }
            return WatchStep::TimedOut;
        }
        let Some(changed_at) = self.changed_at else {
            return WatchStep::Wait;
        };
        let due = match self.read_at {
            None => changed_at + Duration::from_millis(CLIPBOARD_SETTLE_MS),
            Some(read_at) => read_at + Duration::from_millis(CLIPBOARD_EMPTY_RETRY_MS),
        };
        if now < due {
            return WatchStep::Wait;
        }
        self.read_at = Some(now);
        WatchStep::Read
    }
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
    /// until the copy lands and settles. The clipboard is never cleared, so a failed
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
        if platform.copy_target_elevated() {
            warn!("foreground window runs elevated; copy not sent");
            return Err(ClipboardError::ElevatedTarget);
        }
        let baseline = platform.clipboard_change_count();
        platform.simulate_copy(target)?;

        let start = Instant::now();
        let mut watch = CopyWatch::new(baseline, start);
        loop {
            thread::sleep(Duration::from_millis(CLIPBOARD_POLL_INTERVAL_MS));

            // Stop polling promptly once superseded so overlapping captures don't
            // keep issuing clipboard reads in parallel.
            if cancel.load(Ordering::SeqCst) {
                debug!("capture cancelled during clipboard poll");
                return Err(ClipboardError::Cancelled);
            }

            match watch.poll(platform.clipboard_change_count(), Instant::now()) {
                WatchStep::Wait => {}
                WatchStep::Read => {
                    if platform.clipboard_is_line_copy() {
                        warn!("copy holds the caret line, not a selection");
                        return Err(ClipboardError::NothingSelected);
                    }
                    // Files first — see read_content.
                    if let Some(paths) = self.file_list() {
                        return crate::files::ingest_files(&paths);
                    }
                    let (text, images) = self.read_flavors()?;
                    if text.is_some() || !images.is_empty() {
                        let content = ClipboardContent { text, images, files: vec![] };
                        info!(
                            "clipboard content arrived in {}ms (text={}, images={})",
                            start.elapsed().as_millis(),
                            content.text.as_ref().map_or(0, |t| t.len()),
                            content.images.len()
                        );
                        return Ok(content);
                    }
                }
                WatchStep::TimedOut => {
                    let elapsed = start.elapsed().as_millis();
                    if watch.landed() {
                        warn!("copy landed but carried no usable content ({elapsed}ms)");
                        return Err(ClipboardError::EmptyCopy);
                    }
                    warn!("no copy detected within {elapsed}ms");
                    return Err(ClipboardError::NoTextAfterCopy);
                }
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
        let (text, images) = self.read_flavors()?;
        let content = ClipboardContent { text, images, files: vec![] };
        if content.is_empty() {
            return Err(ClipboardError::NoTextInClipboard);
        }

        if let Some(ref t) = content.text {
            info!("read clipboard text ({} chars)", t.len());
        }
        Ok(content)
    }

    /// Text and images across the flavors: the plain-text flavor, the image
    /// flavor, and, only when the image flavor is empty, the images the HTML
    /// flavor references (a rich-text selection carries its pictures solely
    /// as markup references).
    fn read_flavors(&mut self) -> Result<(Option<String>, Vec<ImageAttachment>), ClipboardError> {
        let text = self.board.get_text().ok().filter(|s| !s.trim().is_empty());
        let mut images = read_image_from_board(&mut self.board)?;
        if images.is_empty()
            && let Ok(html) = self.board.get().html()
            && !html.trim().is_empty()
        {
            images = images_from_html(&html, &fetch_source);
            if !images.is_empty() {
                info!("html flavor: {} image(s) attached", images.len());
            }
        }
        Ok((text, images))
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
        /// A second write this long after the first, as multi-step sources do.
        second_write: Option<(Duration, String)>,
        elevated: bool,
        line_copy: bool,
        /// Number of simulate_copy calls.
        copies: std::sync::atomic::AtomicUsize,
    }

    impl Default for MockPlatform {
        fn default() -> Self {
            Self {
                copy_result: Ok(()),
                copy_text: None,
                second_write: None,
                elevated: false,
                line_copy: false,
                copies: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl Platform for MockPlatform {
        fn simulate_copy(&self, _target: Option<i32>) -> Result<(), PlatformError> {
            self.copies.fetch_add(1, Ordering::SeqCst);
            self.copy_result
                .as_ref()
                .map_err(|e| PlatformError::CopyFailed(e.to_string()))?;
            if let Some(text) = &self.copy_text {
                let mut board = Clipboard::new().unwrap();
                board.set_text(text).unwrap();
            }
            if let Some((delay, text)) = self.second_write.clone() {
                thread::spawn(move || {
                    thread::sleep(delay);
                    Clipboard::new().unwrap().set_text(text).unwrap();
                });
            }
            Ok(())
        }

        // The mock writes through the real system clipboard, so delegate to
        // the real platform counter to observe those writes.
        fn clipboard_change_count(&self) -> u64 {
            crate::platform::NativePlatform.clipboard_change_count()
        }

        fn copy_target_elevated(&self) -> bool {
            self.elevated
        }

        fn clipboard_is_line_copy(&self) -> bool {
            self.line_copy
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
    fn read_content_takes_images_from_the_html_flavor() {
        let _lock = lock_clipboard();
        // A 600x400 non-flat PNG inline in the markup, next to plain text.
        let img = image::RgbaImage::from_fn(600, 400, |x, y| {
            if x > 100 && x < 300 && y > 100 && y < 300 {
                image::Rgba([0, 0, 0, 255])
            } else {
                image::Rgba([(x % 256) as u8, (y % 256) as u8, 200, 255])
            }
        });
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let stub = ImageAttachment::stub(png.into_inner());
        let html = format!("<p>caption</p><img src=\"{}\"><img src=\"https://h/emoji/x.png\">", stub.data_uri());

        let mut mgr = ClipboardManager::new().unwrap();
        mgr.board.set().html(html.as_str(), Some("caption")).unwrap();
        let content = mgr.read_content().unwrap();
        assert_eq!(content.text.as_deref(), Some("caption"));
        assert_eq!(content.images.len(), 1, "{:?}", content.images.iter().map(|i| (i.width, i.height)).collect::<Vec<_>>());
        assert_eq!((content.images[0].width, content.images[0].height), (600, 400));
        assert_eq!(content.images[0].origin, ImageOrigin::Markup);
    }

    #[test]
    fn guard_capture_turns_a_panic_into_an_error() {
        let ok = guard_capture(|| Ok(ClipboardContent::text_only("x".into())));
        assert_eq!(ok.unwrap().text.as_deref(), Some("x"));
        let err = guard_capture(|| -> Result<ClipboardContent, ClipboardError> { panic!("boom {}", 42) });
        match err {
            Err(ClipboardError::AccessFailed(msg)) => assert!(msg.contains("panicked") && msg.contains("boom 42"), "{msg}"),
            other => panic!("expected AccessFailed, got {other:?}"),
        }
        let passthrough = guard_capture(|| Err(ClipboardError::EmptyCopy));
        assert!(matches!(passthrough, Err(ClipboardError::EmptyCopy)));
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
            ..MockPlatform::default()
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
            ..MockPlatform::default()
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
            ..MockPlatform::default()
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
            ..MockPlatform::default()
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
            ..MockPlatform::default()
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
            images: vec![ImageAttachment::stub(vec![0x89, 0x50, 0x4E, 0x47])],
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

    // -- CopyWatch tests --

    /// Drive `watch` with `count` at `from..=to` ms in poll-interval steps and
    /// return the offsets at which it asked for a read, plus the final step.
    fn drive(watch: &mut CopyWatch, t0: Instant, count: u64, from: u64, to: u64) -> (Vec<u64>, WatchStep) {
        let mut reads = vec![];
        let mut last = WatchStep::Wait;
        let mut t = from;
        while t <= to {
            last = watch.poll(count, t0 + Duration::from_millis(t));
            match last {
                WatchStep::Read => reads.push(t),
                WatchStep::TimedOut => break,
                WatchStep::Wait => {}
            }
            t += CLIPBOARD_POLL_INTERVAL_MS;
        }
        (reads, last)
    }

    #[test]
    fn copy_watch_times_out_when_nothing_lands() {
        let t0 = Instant::now();
        let mut watch = CopyWatch::new(7, t0);
        let (reads, last) = drive(&mut watch, t0, 7, 2, CLIPBOARD_POLL_TIMEOUT_MS - 2);
        assert!(reads.is_empty());
        assert_eq!(last, WatchStep::Wait);
        assert_eq!(watch.poll(7, t0 + Duration::from_millis(CLIPBOARD_POLL_TIMEOUT_MS)), WatchStep::TimedOut);
        assert!(!watch.landed());
    }

    #[test]
    fn copy_watch_reads_once_the_counter_settles() {
        let t0 = Instant::now();
        let mut watch = CopyWatch::new(7, t0);
        // Lands at 10 ms: no read before the settle period has passed.
        let (reads, _) = drive(&mut watch, t0, 8, 10, 10 + CLIPBOARD_SETTLE_MS);
        assert_eq!(reads, vec![10 + CLIPBOARD_SETTLE_MS]);
        assert!(watch.landed());
    }

    #[test]
    fn copy_watch_restarts_the_settle_on_a_second_write() {
        let t0 = Instant::now();
        let mut watch = CopyWatch::new(7, t0);
        let (reads, _) = drive(&mut watch, t0, 8, 10, 24);
        assert!(reads.is_empty());
        // A second write 16 ms after the first pushes the read out.
        let (reads, _) = drive(&mut watch, t0, 9, 26, 26 + CLIPBOARD_SETTLE_MS);
        assert_eq!(reads, vec![26 + CLIPBOARD_SETTLE_MS]);
    }

    #[test]
    fn copy_watch_retries_an_empty_read_without_a_counter_change() {
        let t0 = Instant::now();
        let mut watch = CopyWatch::new(7, t0);
        let first = 10 + CLIPBOARD_SETTLE_MS;
        let (reads, _) = drive(&mut watch, t0, 8, 10, first + 2 * CLIPBOARD_EMPTY_RETRY_MS);
        assert_eq!(
            reads,
            vec![first, first + CLIPBOARD_EMPTY_RETRY_MS, first + 2 * CLIPBOARD_EMPTY_RETRY_MS]
        );
    }

    #[test]
    fn copy_watch_reads_a_late_copy_once_at_the_deadline() {
        let t0 = Instant::now();
        let mut watch = CopyWatch::new(7, t0);
        let late = CLIPBOARD_POLL_TIMEOUT_MS - 10;
        let (reads, last) = drive(&mut watch, t0, 8, late, CLIPBOARD_POLL_TIMEOUT_MS + 20);
        // Settle would end past the deadline: one read at the deadline, then stop.
        assert_eq!(reads, vec![CLIPBOARD_POLL_TIMEOUT_MS]);
        assert_eq!(last, WatchStep::TimedOut);
        assert!(watch.landed());
        // A counter that keeps moving past the deadline gets no further reads.
        assert_eq!(
            watch.poll(9, t0 + Duration::from_millis(CLIPBOARD_POLL_TIMEOUT_MS + 30)),
            WatchStep::TimedOut
        );
    }

    #[test]
    fn copy_and_read_takes_the_last_of_two_writes() {
        let _lock = lock_clipboard();
        let mut mgr = ClipboardManager::new().unwrap();
        let mock = MockPlatform {
            copy_text: Some("first write".into()),
            second_write: Some((Duration::from_millis(15), "second write".into())),
            ..MockPlatform::default()
        };
        let content = mgr.copy_and_read(&mock, &AtomicBool::new(false), None).unwrap();
        assert_eq!(content.text.as_deref(), Some("second write"));
    }

    #[test]
    fn copy_and_read_refuses_an_elevated_target_without_sending_the_copy() {
        let _lock = lock_clipboard();
        let mut mgr = ClipboardManager::new().unwrap();
        let mock = MockPlatform {
            copy_text: Some("would be copied".into()),
            elevated: true,
            ..MockPlatform::default()
        };

        mgr.write_text("precious content").unwrap();
        let result = mgr.copy_and_read(&mock, &AtomicBool::new(false), None);
        assert!(matches!(result, Err(ClipboardError::ElevatedTarget)), "{result:?}");
        assert_eq!(mock.copies.load(Ordering::SeqCst), 0);
        assert_eq!(mgr.read_clipboard().unwrap(), "precious content");
    }

    #[test]
    fn copy_and_read_rejects_an_editor_line_copy_as_nothing_selected() {
        let _lock = lock_clipboard();
        let mut mgr = ClipboardManager::new().unwrap();
        let mock = MockPlatform {
            copy_text: Some("the caret line\n".into()),
            line_copy: true,
            ..MockPlatform::default()
        };

        let result = mgr.copy_and_read(&mock, &AtomicBool::new(false), None);
        assert!(matches!(result, Err(ClipboardError::NothingSelected)), "{result:?}");
    }

    // -- copy_and_read modifier-wait integration tests --

    #[test]
    fn copy_and_read_fast_path_when_modifiers_already_released() {
        let _lock = lock_clipboard();
        let mgr = ClipboardManager::new().unwrap();
        let mock = MockPlatform {
            copy_result: Ok(()),
            copy_text: Some("fast selection".into()),
            ..MockPlatform::default()
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
            ..MockPlatform::default()
        };

        // Attached but never marked active (simulates a watcher that failed
        // to install, or a build with no watcher at all).
        let modifier_state = ModifierState::default();
        let mut mgr = mgr.with_modifier_state(modifier_state);

        let content = mgr.copy_and_read(&mock, &AtomicBool::new(false), None).unwrap();
        assert_eq!(content.text.as_deref(), Some("selection"));
    }
}
