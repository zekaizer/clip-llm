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
mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacOsPlatform as NativePlatform;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::WindowsPlatform as NativePlatform;
