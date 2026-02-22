use std::thread;
use std::time::Duration;

use tracing::debug;
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
}

/// Show the clip-llm window without activating or stealing focus.
///
/// Uses `SW_SHOWNA` so WM_PAINT is delivered (hidden windows don't receive it),
/// while keeping the foreground window unchanged — this is critical because
/// `SendInput(Ctrl+C)` targets the foreground window for copy simulation.
///
/// Called from coordinator / diagnostics threads before sending actions to UI.
pub fn show_no_activate() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, ShowWindowAsync, SW_SHOWNA};
    let title: Vec<u16> = "clip-llm\0".encode_utf16().collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if !hwnd.is_null() {
        unsafe {
            ShowWindowAsync(hwnd, SW_SHOWNA);
        }
    }
}

/// Show and focus the clip-llm window from any thread.
///
/// Uses `ShowWindowAsync` (PostMessage-based, cross-thread safe) + `SetForegroundWindow`.
/// Called from `show_window()` in the UI after clipboard content is ready.
pub fn show_and_focus_window() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, ShowWindowAsync, SW_SHOW,
    };
    let title: Vec<u16> = "clip-llm\0".encode_utf16().collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if !hwnd.is_null() {
        unsafe {
            ShowWindowAsync(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
        }
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
