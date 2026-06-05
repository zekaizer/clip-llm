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

    /// Show the overlay at an optional position WITHOUT taking keyboard focus, so
    /// the user's app stays key and a simulated Cmd+C/Ctrl+C still targets it.
    /// Used while capturing the current selection on double-tap.
    /// Returns true if an egui `Visible(true)` viewport sync is also needed.
    fn show_window_no_activate(&self, pos: Option<(f32, f32)>) -> bool;

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

// -- System tray (Windows taskbar / macOS menu bar) --
//
// `tray-icon` is cross-platform, so the implementation is shared here; only the
// Windows-specific window nudge in the menu handler is `cfg`-gated.

use std::sync::atomic::{AtomicBool, Ordering};

static TRAY_QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Decode the embedded tray icon PNG into an RGBA `tray_icon::Icon`.
fn load_tray_icon() -> tray_icon::Icon {
    let png_bytes = include_bytes!("../../assets/tray-icon-32.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes.as_slice()));
    let mut reader = decoder.read_info().expect("invalid tray icon PNG");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("unknown output buffer size")];
    let info = reader.next_frame(&mut buf).expect("failed to decode tray icon");
    buf.truncate(info.buffer_size());
    tray_icon::Icon::from_rgba(buf, info.width, info.height).expect("invalid RGBA icon data")
}

/// Initialize the system tray icon with a disabled version label and a Quit item.
/// On macOS this is the only way to quit the app (Accessory policy = no Dock icon).
/// The `TrayIcon` is intentionally leaked (process-lifetime resource).
pub fn init_tray(ctx: &eframe::egui::Context) {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::TrayIconBuilder;

    let about_item = MenuItem::new("About clip-llm", true, None);
    let about_id = about_item.id().clone();
    let quit_item = MenuItem::new("Quit", true, None);
    let quit_id = quit_item.id().clone();
    let menu = Menu::with_items(&[&about_item, &PredefinedMenuItem::separator(), &quit_item])
        .expect("failed to create tray menu");

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("clip-llm")
        .with_icon(load_tray_icon())
        .build();

    match tray {
        Ok(tray) => {
            // Leak: the tray icon lives for the entire process lifetime.
            std::mem::forget(tray);

            // set_event_handler intercepts all menu events; compare the Quit id and
            // signal via AtomicBool so poll_tray_quit() can act inside update().
            let quit_id = quit_id.clone();
            let about_id = about_id.clone();
            let ctx = ctx.clone();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                if event.id() == &quit_id {
                    TRAY_QUIT_REQUESTED.store(true, Ordering::SeqCst);
                    // Windows: a hidden window gets no WM_PAINT, so nudge it visible
                    // so update() runs and poll_tray_quit() can act. Not needed on macOS.
                    #[cfg(target_os = "windows")]
                    windows::show_no_activate();
                    ctx.request_repaint();
                } else if event.id() == &about_id {
                    // Menu events fire on the main thread, so showing the panel
                    // directly is safe (no eframe ctx needed).
                    #[cfg(target_os = "macos")]
                    macos::show_about();
                    #[cfg(target_os = "windows")]
                    windows::show_about();
                }
            }));

            tracing::info!("system tray icon created");
        }
        Err(e) => {
            tracing::warn!("failed to create tray icon: {e}");
        }
    }
}

/// Poll for the tray quit flag; sends `ViewportCommand::Close` when set.
pub fn poll_tray_quit(ctx: &eframe::egui::Context) {
    if TRAY_QUIT_REQUESTED.swap(false, Ordering::SeqCst) {
        tracing::info!("quit requested from tray menu");
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
    }
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
