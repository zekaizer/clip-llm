use std::ffi::{c_char, c_ulong, c_void};
use std::thread;
use std::time::Duration;

use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::{CGPoint, CGRect};
use tracing::{debug, info, warn};

use super::Platform;
use crate::PlatformError;

/// Virtual key code for 'C' on ANSI keyboards.
const KEY_C: CGKeyCode = 0x08;

/// Delay between key-down and key-up events (ms).
const KEY_EVENT_DELAY_MS: u64 = 50;

/// NSWindowCollectionBehavior flags.
const NS_WINDOW_COLLECTION_BEHAVIOR_MOVE_TO_ACTIVE_SPACE: c_ulong = 1 << 1;
const NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY: c_ulong = 1 << 8;

#[link(name = "AppKit", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "objc", kind = "dylib")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend(obj: *mut c_void, sel: *mut c_void) -> *mut c_void;
}

/// Get the first NSWindow from [NSApp windows].
/// Returns null if unavailable.
unsafe fn get_app_window() -> *mut c_void {
    let cls = objc_getClass(c"NSApplication".as_ptr());
    if cls.is_null() {
        return std::ptr::null_mut();
    }
    let app = objc_msgSend(cls, sel_registerName(c"sharedApplication".as_ptr()));
    if app.is_null() {
        return std::ptr::null_mut();
    }
    let windows = objc_msgSend(app, sel_registerName(c"windows".as_ptr()));
    if windows.is_null() {
        return std::ptr::null_mut();
    }
    objc_msgSend(windows, sel_registerName(c"firstObject".as_ptr()))
}

/// Configure the NSWindow for overlay use:
/// - Moves to active Space and can appear over fullscreen apps.
/// - Disables native macOS window shadow (we draw our own via egui Frame).
/// Returns true if successfully configured.
pub fn configure_window_for_spaces() -> bool {
    type MsgSendUlong = unsafe extern "C" fn(*mut c_void, *mut c_void, c_ulong);
    let msg_send_ulong: MsgSendUlong = unsafe { std::mem::transmute(objc_msgSend as *const ()) };

    type MsgSendBool = unsafe extern "C" fn(*mut c_void, *mut c_void, bool);
    let msg_send_bool: MsgSendBool = unsafe { std::mem::transmute(objc_msgSend as *const ()) };

    unsafe {
        let window = get_app_window();
        if window.is_null() {
            warn!("failed to get app window for Spaces config");
            return false;
        }
        let behavior = NS_WINDOW_COLLECTION_BEHAVIOR_MOVE_TO_ACTIVE_SPACE
            | NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY;
        let sel_set = sel_registerName(c"setCollectionBehavior:".as_ptr());
        msg_send_ulong(window, sel_set, behavior);

        // Disable native macOS window shadow. winit defaults hasShadow=YES even
        // for transparent windows, which creates a visible gray outline around
        // the overlay. The egui Frame renders its own shadow inside the window.
        let sel_shadow = sel_registerName(c"setHasShadow:".as_ptr());
        msg_send_bool(window, sel_shadow, false);

        debug!("configured NSWindow for Spaces + disabled native shadow");
        true
    }
}


extern "C" {
    fn CGGetDisplaysWithPoint(
        point: CGPoint,
        max_displays: u32,
        displays: *mut u32,
        matching_display_count: *mut u32,
    ) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
}

/// Get the display bounds (Quartz coordinates) of the screen containing the given point.
/// Returns (origin_x, origin_y, width, height).
pub fn display_bounds_at_point(x: f64, y: f64) -> Option<(f64, f64, f64, f64)> {
    let point = CGPoint::new(x, y);
    let mut display: u32 = 0;
    let mut count: u32 = 0;
    unsafe {
        let err = CGGetDisplaysWithPoint(point, 1, &mut display, &mut count);
        if err != 0 || count == 0 {
            return None;
        }
        let bounds = CGDisplayBounds(display);
        Some((
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.width,
            bounds.size.height,
        ))
    }
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

    fn mouse_position(&self) -> Option<(f64, f64)> {
        // CGEvent.location() returns Quartz logical points (top-left origin).
        // This matches egui's OuterPosition coordinate system directly.
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
        let event = CGEvent::new(source).ok()?;
        let pos = event.location();
        Some((pos.x, pos.y))
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
