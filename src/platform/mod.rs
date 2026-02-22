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

/// Returns a platform-specific callback for pre-show hooks (coordinator / diagnostics threads).
///
/// On Windows, hidden windows (SW_HIDE) do not receive WM_PAINT, so eframe `update()`
/// never fires. This callback uses `SW_SHOWNA` to make the window visible without
/// stealing focus — keeping `SendInput(Ctrl+C)` targeting the correct foreground window.
///
/// On macOS, no-op — macOS uses `CGEvent` for copy simulation (focus-independent).
pub fn pre_show_callback() -> Box<dyn Fn() + Send> {
    #[cfg(target_os = "windows")]
    {
        Box::new(|| windows::show_no_activate())
    }
    #[cfg(not(target_os = "windows"))]
    {
        Box::new(|| {})
    }
}
