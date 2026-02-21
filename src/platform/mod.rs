/// Platform abstraction trait for OS-specific operations.
pub trait Platform {
    /// Simulate Cmd+C (macOS) or Ctrl+C (Windows) to copy selected text.
    fn simulate_copy(&self) -> Result<(), crate::PlatformError>;

    /// Check and prompt for required OS permissions (e.g. macOS Accessibility).
    fn check_accessibility(&self) -> Result<(), crate::PlatformError>;
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacOsPlatform as NativePlatform;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::WindowsPlatform as NativePlatform;

/// Run the platform event loop. Calls `tick` on each iteration.
/// This function never returns under normal operation.
#[cfg(target_os = "macos")]
pub fn run_event_loop(mut tick: impl FnMut()) -> ! {
    macos::run_event_loop_impl(&mut tick)
}

#[cfg(target_os = "windows")]
pub fn run_event_loop(mut tick: impl FnMut()) -> ! {
    windows::run_event_loop_impl(&mut tick)
}
