/// Platform abstraction trait for OS-specific operations.
pub trait Platform {
    /// Simulate Cmd+C (macOS) or Ctrl+C (Windows) to copy selected text.
    fn simulate_copy(&self) -> Result<(), crate::PlatformError>;

    /// Check and prompt for required OS permissions (e.g. macOS Accessibility).
    fn check_accessibility(&self) -> Result<(), crate::PlatformError>;

    /// Get the current mouse cursor position in screen coordinates (egui logical points).
    fn mouse_position(&self) -> Option<(f64, f64)>;

    /// Get the display work area (logical points) of the monitor containing the given point.
    /// Returns (origin_x, origin_y, width, height). Work area excludes taskbar/dock.
    fn display_bounds_at_point(&self, x: f64, y: f64) -> Option<(f64, f64, f64, f64)>;

    /// Show and focus the overlay window at an optional position.
    /// Returns true if an egui `Visible(true)` viewport sync is also needed
    /// (Windows winit workaround to maintain ControlFlow::Wait, egui#5229).
    fn show_window(&self, pos: Option<(f32, f32)>) -> bool;

    /// Hide the overlay window. Returns true if handled natively (caller must not send
    /// `Visible(false)`); false means the caller should send `ViewportCommand::Visible(false)`.
    fn hide_window(&self) -> bool;

    /// Reposition the window using a direct native API call.
    /// Returns true if handled natively (caller must not send `OuterPosition`).
    fn reposition_window(&self, x: f32, y: f32) -> bool;

    /// Paste clipboard content into the previously focused application.
    /// Handles focus transfer, timing, key simulation, and platform-specific cleanup.
    fn paste_to_foreground(&self) -> Result<(), crate::PlatformError>;
}

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacOsPlatform as NativePlatform;

#[cfg(target_os = "windows")]
pub(crate) mod windows;
#[cfg(target_os = "windows")]
pub use windows::WindowsPlatform as NativePlatform;

/// Initialize the system tray icon with a Quit menu.
/// On Windows, creates a tray icon and sets up event handling.
/// On macOS, no-op (tray support planned for future).
pub fn init_tray(_ctx: &eframe::egui::Context) {
    #[cfg(target_os = "windows")]
    windows::init_tray(_ctx);
}

/// Poll system tray events (e.g. Quit menu click).
/// On Windows, checks for pending tray menu events.
/// On macOS, no-op.
pub fn poll_tray_quit(_ctx: &eframe::egui::Context) {
    #[cfg(target_os = "windows")]
    windows::poll_tray_quit(_ctx);
}

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
        Box::new(windows::show_no_activate)
    }
    #[cfg(not(target_os = "windows"))]
    {
        Box::new(|| {})
    }
}
