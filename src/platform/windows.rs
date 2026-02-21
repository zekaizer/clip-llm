use std::thread;
use std::time::Duration;

use tracing::debug;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_C, VK_CONTROL,
};

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
