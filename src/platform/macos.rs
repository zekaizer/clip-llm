use std::thread;
use std::time::Duration;

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

/// Timeout for ReceiveNextEvent in seconds.
const EVENT_TIMEOUT_SECS: f64 = 0.05;

#[link(name = "AppKit", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn GetApplicationEventTarget() -> *mut std::ffi::c_void;
    fn ReceiveNextEvent(
        num_types: u32,
        list: *const std::ffi::c_void,
        timeout: f64,
        pull: u8,
        event: *mut *mut std::ffi::c_void,
    ) -> i32;
    fn SendEventToEventTarget(
        event: *mut std::ffi::c_void,
        target: *mut std::ffi::c_void,
    ) -> i32;
    fn ReleaseEvent(event: *mut std::ffi::c_void);
}

extern "C" {
    fn objc_getClass(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
    fn sel_registerName(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
    fn objc_msgSend(receiver: *mut std::ffi::c_void, sel: *mut std::ffi::c_void, ...) -> *mut std::ffi::c_void;
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

/// Initialize NSApplication so Carbon hotkey events are delivered in CLI binaries.
pub(super) fn init_impl() {
    unsafe {
        let cls = objc_getClass(c"NSApplication".as_ptr());
        let shared_app_sel = sel_registerName(c"sharedApplication".as_ptr());
        let app = objc_msgSend(cls, shared_app_sel);

        // setActivationPolicy: NSApplicationActivationPolicyProhibited (2)
        // Prevents Dock icon while still receiving Carbon events.
        let set_policy_sel = sel_registerName(c"setActivationPolicy:".as_ptr());
        objc_msgSend(app, set_policy_sel, 2i64);
    }
    info!("NSApplication initialized for Carbon event delivery");
}

/// Dispatch Carbon events via ReceiveNextEvent, then call `tick`.
/// This replaces CFRunLoop which cannot dispatch Carbon Application Events
/// that global-hotkey registers via RegisterEventHotKey.
pub(super) fn run_event_loop_impl(tick: &mut dyn FnMut()) -> ! {
    loop {
        unsafe {
            let mut event = std::ptr::null_mut();
            let status = ReceiveNextEvent(
                0,
                std::ptr::null(),
                EVENT_TIMEOUT_SECS,
                1, // pull = true (remove from queue)
                &mut event,
            );
            if status == 0 {
                SendEventToEventTarget(event, GetApplicationEventTarget());
                ReleaseEvent(event);
            }
        }
        tick();
    }
}
