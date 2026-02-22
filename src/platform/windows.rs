use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use eframe::egui;
use tracing::{debug, info, warn};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_C, VK_CONTROL,
    VK_SHIFT,
};
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

use super::Platform;
use crate::PlatformError;

/// Delay between key events (ms).
const KEY_EVENT_DELAY_MS: u64 = 50;

pub struct WindowsPlatform;

impl Platform for WindowsPlatform {
    /// Simulate Ctrl+C by sending keyboard input via SendInput.
    fn simulate_copy(&self) -> Result<(), PlatformError> {
        debug!("posting Ctrl+C key events via SendInput");

        let inputs = [
            // Release any held modifier keys from the hotkey (Ctrl+Shift+C).
            // SendInput merges with physical key state — if Shift is still
            // held, the OS would see Ctrl+Shift+C instead of Ctrl+C.
            make_key_input(VK_SHIFT, KEYEVENTF_KEYUP),
            make_key_input(VK_CONTROL, KEYEVENTF_KEYUP),
            // Clean Ctrl+C
            make_key_input(VK_CONTROL, 0),
            make_key_input(VK_C, 0),
            make_key_input(VK_C, KEYEVENTF_KEYUP),
            make_key_input(VK_CONTROL, KEYEVENTF_KEYUP),
        ];

        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };

        if sent != inputs.len() as u32 {
            return Err(PlatformError::CopyFailed(format!(
                "SendInput returned {sent}, expected {}",
                inputs.len()
            )));
        }

        thread::sleep(Duration::from_millis(KEY_EVENT_DELAY_MS));
        Ok(())
    }

    fn check_accessibility(&self) -> Result<(), PlatformError> {
        // No special permission required on Windows.
        Ok(())
    }

    fn mouse_position(&self) -> Option<(f64, f64)> {
        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut pt) } == 0 {
            return None;
        }
        // GetCursorPos returns physical pixels in DPI-aware processes.
        // Convert to logical points for egui OuterPosition.
        let dpi = unsafe { GetDpiForSystem() } as f64;
        let scale = dpi / 96.0;
        Some((pt.x as f64 / scale, pt.y as f64 / scale))
    }

    fn display_bounds_at_point(&self, x: f64, y: f64) -> Option<(f64, f64, f64, f64)> {
        use windows_sys::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        };
        let dpi = unsafe { GetDpiForSystem() } as f64;
        let scale = dpi / 96.0;
        // Convert logical points to physical pixels for MonitorFromPoint.
        let pt = POINT {
            x: (x * scale) as i32,
            y: (y * scale) as i32,
        };
        let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
        if hmon.is_null() {
            return None;
        }
        let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if unsafe { GetMonitorInfoW(hmon, &mut info) } == 0 {
            return None;
        }
        // rcWork = work area (excludes taskbar). Convert back to logical points.
        let rc = info.rcWork;
        Some((
            rc.left as f64 / scale,
            rc.top as f64 / scale,
            (rc.right - rc.left) as f64 / scale,
            (rc.bottom - rc.top) as f64 / scale,
        ))
    }
}

/// Show the clip-llm window without activating or stealing focus.
///
/// Uses `SW_SHOWNA` so WM_PAINT is delivered (hidden windows don't receive it),
/// while keeping the foreground window unchanged — this is critical because
/// `SendInput(Ctrl+C)` targets the foreground window for copy simulation.
///
/// Called from coordinator / diagnostics threads before sending actions to UI.
pub fn show_no_activate() {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetWindowRect, SetWindowPos, ShowWindowAsync, HWND_TOP, SW_SHOWNA,
        SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };
    let title: Vec<u16> = "clip-llm\0".encode_utf16().collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if !hwnd.is_null() {
        unsafe {
            // Center window on cursor before showing to prevent flash.
            // Both GetCursorPos and GetWindowRect return physical pixels,
            // so no DPI conversion is needed for the centering offset.
            let mut pt = POINT { x: 0, y: 0 };
            let mut rect: RECT = std::mem::zeroed();
            if GetCursorPos(&mut pt) != 0 && GetWindowRect(hwnd, &mut rect) != 0 {
                let w = rect.right - rect.left;
                let h = rect.bottom - rect.top;
                SetWindowPos(
                    hwnd,
                    HWND_TOP,
                    pt.x - w / 2,
                    pt.y - h / 2,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            ShowWindowAsync(hwnd, SW_SHOWNA);
        }
    }
}

/// Show and focus the clip-llm window from any thread.
///
/// If `position` is provided (logical points), the window is moved there
/// **synchronously** via `SetWindowPos` before becoming visible. This
/// eliminates the one-frame flash at the wrong location.
///
/// Uses `ShowWindowAsync` (PostMessage-based, cross-thread safe) + `SetForegroundWindow`.
/// Called from `show_window()` in the UI after clipboard content is ready.
pub fn show_and_focus_window(position: Option<(f32, f32)>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, SetWindowPos, ShowWindowAsync, HWND_TOP, SW_SHOW,
        SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };
    let title: Vec<u16> = "clip-llm\0".encode_utf16().collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if !hwnd.is_null() {
        unsafe {
            if let Some((x, y)) = position {
                let dpi = GetDpiForSystem() as f64;
                let scale = dpi / 96.0;
                SetWindowPos(
                    hwnd,
                    HWND_TOP,
                    (x as f64 * scale) as i32,
                    (y as f64 * scale) as i32,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            ShowWindowAsync(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
        }
    }
}

// -- System tray --

static TRAY_QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Decode the embedded tray icon PNG into an RGBA `tray_icon::Icon`.
fn load_tray_icon() -> tray_icon::Icon {
    let png_bytes = include_bytes!("../../assets/tray-icon-32.png");
    let decoder = png::Decoder::new(png_bytes.as_slice());
    let mut reader = decoder.read_info().expect("invalid tray icon PNG");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("failed to decode tray icon");
    buf.truncate(info.buffer_size());
    tray_icon::Icon::from_rgba(buf, info.width, info.height).expect("invalid RGBA icon data")
}

/// Create the system tray icon with a Quit menu item.
///
/// The `TrayIcon` is intentionally leaked (process-lifetime resource) so that
/// `OverlayApp` does not need to hold it.
pub fn init_tray(ctx: &egui::Context) {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::TrayIconBuilder;

    let quit_item = MenuItem::new("Quit", true, None);
    let quit_id = quit_item.id().clone();
    let menu = Menu::with_items(&[&quit_item]).expect("failed to create tray menu");
    let icon = load_tray_icon();

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("clip-llm")
        .with_icon(icon)
        .build();

    match tray {
        Ok(tray) => {
            // Leak: tray icon lives for the entire process lifetime.
            std::mem::forget(tray);

            // set_event_handler intercepts all events — MenuEvent::receiver()
            // channel stays empty. Compare quit ID inside the handler and
            // signal via AtomicBool so poll_tray_quit() can act in update().
            let quit_id_for_handler = quit_id.clone();
            let ctx = ctx.clone();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                if event.id() == &quit_id_for_handler {
                    TRAY_QUIT_REQUESTED.store(true, Ordering::SeqCst);
                }
                show_no_activate();
                ctx.request_repaint();
            }));

            info!("system tray icon created");
        }
        Err(e) => {
            warn!("failed to create tray icon: {e}");
        }
    }
}

/// Poll for tray quit flag. Sends `ViewportCommand::Close` when set.
pub fn poll_tray_quit(ctx: &egui::Context) {
    if TRAY_QUIT_REQUESTED.swap(false, Ordering::SeqCst) {
        info!("quit requested from tray menu");
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

fn make_key_input(vk: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
