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
/// Virtual key code for 'V' on ANSI keyboards.
const KEY_V: CGKeyCode = 0x09;

// Typed function pointer aliases for `objc_msgSend` transmute casts.
type MsgSendBool = unsafe extern "C" fn(*mut c_void, *mut c_void, bool);
type MsgSendUlong = unsafe extern "C" fn(*mut c_void, *mut c_void, c_ulong);
type MsgSendPoint = unsafe extern "C" fn(*mut c_void, *mut c_void, CGPoint);
type MsgSendPtr = unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void);
type MsgSendRetI64 = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i64;
type MsgSendRetBool = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;

/// Delay between key-down and key-up events (ms).
const KEY_EVENT_DELAY_MS: u64 = 50;
/// Delay for WindowServer focus transfer after [NSApp hide:] (ms).
const FOCUS_TRANSFER_DELAY_MS: u64 = 100;

/// NSWindowCollectionBehavior flags.
const NS_WINDOW_COLLECTION_BEHAVIOR_MOVE_TO_ACTIVE_SPACE: c_ulong = 1 << 1;
const NS_WINDOW_COLLECTION_BEHAVIOR_TRANSIENT: c_ulong = 1 << 3;
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
/// - MoveToActiveSpace: window moves to the current Space when shown.
/// - Transient: excluded from Dock/Exposé.
/// - FullScreenAuxiliary: can appear alongside fullscreen apps.
/// - Disables native macOS window shadow (we draw our own via egui Frame).
///
/// Returns true if successfully configured.
pub fn configure_window_for_spaces() -> bool {
    let msg_send_ulong: MsgSendUlong = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    let msg_send_bool: MsgSendBool = unsafe { std::mem::transmute(objc_msgSend as *const ()) };

    unsafe {
        let window = get_app_window();
        if window.is_null() {
            warn!("failed to get app window for Spaces config");
            return false;
        }
        let behavior = NS_WINDOW_COLLECTION_BEHAVIOR_MOVE_TO_ACTIVE_SPACE
            | NS_WINDOW_COLLECTION_BEHAVIOR_TRANSIENT
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

/// Show the NSWindow and activate the app so it receives focus properly.
///
/// If `position` is provided (Quartz coordinates: top-left origin), the window
/// is moved there **synchronously** via `setFrameTopLeftPoint:` before becoming
/// visible. This eliminates the one-frame flash at the wrong location that
/// occurs when using async `ViewportCommand::OuterPosition`.
///
/// For Accessory-policy apps, `activateIgnoringOtherApps:` is safe because
/// Accessory apps have no "home Space" — macOS will not switch Spaces.
pub fn show_and_focus_window(position: Option<(f32, f32)>) {
    let msg_send_bool: MsgSendBool = unsafe { std::mem::transmute(objc_msgSend as *const ()) };

    unsafe {
        let window = get_app_window();
        if window.is_null() {
            return;
        }

        // Synchronous pre-positioning: convert Quartz (top-left origin) to
        // Cocoa screen coordinates (bottom-left origin) and set before showing.
        if let Some((x, y)) = position {
            let screen_height = CGDisplayBounds(CGMainDisplayID()).size.height;
            let cocoa_point = CGPoint::new(x as f64, screen_height - y as f64);
            let msg_send_point: MsgSendPoint = std::mem::transmute(objc_msgSend as *const ());
            msg_send_point(
                window,
                sel_registerName(c"setFrameTopLeftPoint:".as_ptr()),
                cocoa_point,
            );
        }

        let nil: *mut c_void = std::ptr::null_mut();
        let msg_send_ptr: MsgSendPtr = std::mem::transmute(objc_msgSend as *const ());

        // orderFront: shows the window without activating the app.
        msg_send_ptr(window, sel_registerName(c"orderFront:".as_ptr()), nil);
        // makeKeyWindow makes it receive keyboard input.
        objc_msgSend(window, sel_registerName(c"makeKeyWindow".as_ptr()));

        // Activate the app so winit reports proper focus state.
        // Safe for Accessory apps — no home Space means no Space switching.
        let cls = objc_getClass(c"NSApplication".as_ptr());
        let app = objc_msgSend(cls, sel_registerName(c"sharedApplication".as_ptr()));
        let sel = sel_registerName(c"activateIgnoringOtherApps:".as_ptr());
        msg_send_bool(app, sel, true);
    }
}


extern "C" {
    fn CGMainDisplayID() -> u32;
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

#[allow(dead_code)]
/// Log NSWindow and NSApp diagnostic info for debugging overlay behavior.
/// Outputs: activation policy, collection behavior bits, window level,
/// visibility, and key/main status.
pub fn log_window_diagnostics() {
    let msg_send_i64: MsgSendRetI64 = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    let msg_send_bool: MsgSendRetBool = unsafe { std::mem::transmute(objc_msgSend as *const ()) };

    unsafe {
        let cls = objc_getClass(c"NSApplication".as_ptr());
        let app = objc_msgSend(cls, sel_registerName(c"sharedApplication".as_ptr()));

        let policy = msg_send_i64(app, sel_registerName(c"activationPolicy".as_ptr()));
        let policy_name = match policy {
            0 => "Regular",
            1 => "Accessory",
            2 => "Prohibited",
            _ => "Unknown",
        };

        let window = get_app_window();
        if window.is_null() {
            info!(
                "window_diag: policy={policy_name}({policy}), window=null"
            );
            return;
        }

        let behavior = msg_send_i64(window, sel_registerName(c"collectionBehavior".as_ptr()));
        let level = msg_send_i64(window, sel_registerName(c"level".as_ptr()));
        let visible = msg_send_bool(window, sel_registerName(c"isVisible".as_ptr()));
        let is_key = msg_send_bool(window, sel_registerName(c"isKeyWindow".as_ptr()));
        let is_main = msg_send_bool(window, sel_registerName(c"isMainWindow".as_ptr()));

        info!(
            "window_diag: policy={policy_name}({policy}), behavior=0x{behavior:x}, \
             level={level}, visible={visible}, key={is_key}, main={is_main}"
        );
    }
}

pub struct MacOsPlatform;

impl MacOsPlatform {
    /// Simulate Cmd+V by posting CGEvent keyboard events to the HID system.
    fn simulate_paste(&self) -> Result<(), PlatformError> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| PlatformError::PasteFailed("failed to create CGEventSource".into()))?;

        let key_down = CGEvent::new_keyboard_event(source.clone(), KEY_V, true)
            .map_err(|()| PlatformError::PasteFailed("failed to create key-down event".into()))?;
        key_down.set_flags(CGEventFlags::CGEventFlagCommand);

        let key_up = CGEvent::new_keyboard_event(source, KEY_V, false)
            .map_err(|()| PlatformError::PasteFailed("failed to create key-up event".into()))?;
        key_up.set_flags(CGEventFlags::CGEventFlagCommand);

        debug!("posting Cmd+V key events to HID");
        key_down.post(CGEventTapLocation::HID);
        thread::sleep(Duration::from_millis(KEY_EVENT_DELAY_MS));
        key_up.post(CGEventTapLocation::HID);

        Ok(())
    }
}

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

    fn display_bounds_at_point(&self, x: f64, y: f64) -> Option<(f64, f64, f64, f64)> {
        display_bounds_at_point(x, y)
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

    fn show_window(&self, pos: Option<(f32, f32)>) -> bool {
        configure_window_for_spaces();
        show_and_focus_window(pos);
        false // no egui Visible(true) sync needed on macOS
    }

    fn hide_window(&self) -> bool {
        false // caller should use ViewportCommand::Visible(false)
    }

    fn reposition_window(&self, _x: f32, _y: f32) -> bool {
        false // caller should use ViewportCommand::OuterPosition
    }

    fn paste_to_foreground(&self) -> Result<(), PlatformError> {
        // Deactivate this app so the OS activates the previously focused app.
        // [NSApp hide:nil] itself is synchronous, but the WindowServer IPC for
        // focus transfer to the target app is asynchronous — the target needs
        // time to become first responder before it can receive key events.
        unsafe {
            let cls = objc_getClass(c"NSApplication".as_ptr());
            let app = objc_msgSend(cls, sel_registerName(c"sharedApplication".as_ptr()));
            if !app.is_null() {
                let nil: *mut c_void = std::ptr::null_mut();
                let msg_send_ptr: MsgSendPtr =
                    std::mem::transmute(objc_msgSend as *const ());
                msg_send_ptr(app, sel_registerName(c"hide:".as_ptr()), nil);
                debug!("yielded focus via [NSApp hide:]");
            }
        }
        // Wait for WindowServer focus transfer to complete.
        thread::sleep(Duration::from_millis(FOCUS_TRANSFER_DELAY_MS));
        self.simulate_paste()
    }
}
