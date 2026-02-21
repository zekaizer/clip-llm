use std::thread;
use std::time::Duration;

use core_foundation::runloop::CFRunLoop;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use tracing::{debug, info};

use super::Platform;
use crate::PlatformError;

/// Virtual key code for 'C' on ANSI keyboards.
const KEY_C: CGKeyCode = 0x08;

/// Delay between key-down and key-up events (ms).
const KEY_EVENT_DELAY_MS: u64 = 50;

/// Interval for CFRunLoop pumping (ms).
const EVENT_LOOP_INTERVAL: Duration = Duration::from_millis(50);

extern "C" {
    fn AXIsProcessTrusted() -> bool;
    static kCFRunLoopDefaultMode: core_foundation::string::CFStringRef;
}

pub struct MacOsPlatform;

impl Platform for MacOsPlatform {
    /// Simulate Cmd+C by posting CGEvent keyboard events to the HID system.
    /// Requires Accessibility permission.
    fn simulate_copy(&self) -> Result<(), PlatformError> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| PlatformError::CopyFailed("failed to create CGEventSource".into()))?;

        let key_down = CGEvent::new_keyboard_event(source.clone(), KEY_C, true)
            .map_err(|()| PlatformError::CopyFailed("failed to create key-down event".into()))?;
        key_down.set_flags(CGEventFlags::CGEventFlagCommand);

        let key_up = CGEvent::new_keyboard_event(source, KEY_C, false)
            .map_err(|()| PlatformError::CopyFailed("failed to create key-up event".into()))?;
        key_up.set_flags(CGEventFlags::CGEventFlagCommand);

        debug!("posting Cmd+C key events to HID");
        key_down.post(CGEventTapLocation::HID);
        thread::sleep(Duration::from_millis(KEY_EVENT_DELAY_MS));
        key_up.post(CGEventTapLocation::HID);

        Ok(())
    }

    /// Check if the process has Accessibility permission.
    /// Returns `AccessibilityDenied` if not granted.
    fn check_accessibility(&self) -> Result<(), PlatformError> {
        let trusted = unsafe { AXIsProcessTrusted() };
        if trusted {
            info!("accessibility permission granted");
            Ok(())
        } else {
            Err(PlatformError::AccessibilityDenied)
        }
    }
}

/// Pump CFRunLoop to deliver Carbon hotkey events, then call `tick`.
pub(super) fn run_event_loop_impl(tick: &mut dyn FnMut()) -> ! {
    loop {
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            EVENT_LOOP_INTERVAL,
            false,
        );
        tick();
    }
}
