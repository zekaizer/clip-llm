/// Platform abstraction trait for OS-specific operations.
pub trait Platform {
    /// Simulate Cmd+C (macOS) or Ctrl+C (Windows) to copy selected text.
    fn simulate_copy(&self) -> Result<(), crate::PlatformError>;

    /// Check and prompt for required OS permissions (e.g. macOS Accessibility).
    fn check_accessibility(&self) -> Result<(), crate::PlatformError>;

    /// Get the current mouse cursor position in screen coordinates (egui logical points).
    fn mouse_position(&self) -> Option<(f64, f64)>;
}

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacOsPlatform as NativePlatform;

#[cfg(target_os = "windows")]
pub(crate) mod windows;
#[cfg(target_os = "windows")]
pub use windows::WindowsPlatform as NativePlatform;

/// Returns a callback that shows and focuses the app window natively.
///
/// On Windows, uses `ShowWindowAsync` + `SetForegroundWindow` (cross-thread safe).
/// On other platforms, returns a no-op (macOS handles show via ObjC in `ui::show_window`).
pub fn pre_show_callback() -> Box<dyn Fn() + Send> {
    #[cfg(target_os = "windows")]
    {
        Box::new(|| windows::show_and_focus_window())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Box::new(|| {})
    }
}
