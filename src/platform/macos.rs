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
type MsgSendRetI32 = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
type MsgSendRetBool = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;
type MsgSendUlongRetPtr = unsafe extern "C" fn(*mut c_void, *mut c_void, c_ulong) -> *mut c_void;
#[cfg(target_arch = "aarch64")]
type MsgSendRetRect = unsafe extern "C" fn(*mut c_void, *mut c_void) -> CGRect;

/// Delay between key-down and key-up events (ms).
const KEY_EVENT_DELAY_MS: u64 = 50;
/// Delay for WindowServer focus transfer after [NSApp hide:] (ms).
const FOCUS_TRANSFER_DELAY_MS: u64 = 100;

/// NSWindowCollectionBehavior flags.
const NS_WINDOW_COLLECTION_BEHAVIOR_MOVE_TO_ACTIVE_SPACE: c_ulong = 1 << 1;
const NS_WINDOW_COLLECTION_BEHAVIOR_TRANSIENT: c_ulong = 1 << 3;
const NS_WINDOW_COLLECTION_BEHAVIOR_FULL_SCREEN_AUXILIARY: c_ulong = 1 << 8;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    /// `options` is a `CFDictionaryRef`; passing `kAXTrustedCheckOptionPrompt =
    /// true` makes macOS show the permission dialog and open System Settings.
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
}

/// Check Accessibility trust using the prompting variant: when permission is not
/// yet granted, macOS shows its standard dialog and opens System Settings >
/// Privacy & Security > Accessibility, instead of failing silently.
fn ax_is_process_trusted_with_prompt() -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;

    // Documented underlying string value of kAXTrustedCheckOptionPrompt.
    let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
    let value = CFBoolean::true_value();
    let options = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef() as *const c_void) }
}

#[link(name = "objc", kind = "dylib")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend(obj: *mut c_void, sel: *mut c_void) -> *mut c_void;
    // Struct-return variant: on x86_64 a 32-byte struct comes back via a hidden
    // sret pointer, which Rust's extern "C" struct return matches exactly.
    // arm64 has no _stret — CGRect (4 doubles) returns in d0-d3 via objc_msgSend.
    #[cfg(target_arch = "x86_64")]
    fn objc_msgSend_stret(obj: *mut c_void, sel: *mut c_void) -> CGRect;
}

/// Send a message returning a `CGRect` (e.g. `frame`, `visibleFrame`),
/// dispatching to the correct ABI per architecture.
unsafe fn msg_send_rect(obj: *mut c_void, sel: *mut c_void) -> CGRect {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let send: MsgSendRetRect = std::mem::transmute(objc_msgSend as *const ());
        send(obj, sel)
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        objc_msgSend_stret(obj, sel)
    }
}

/// Get the first NSWindow from [NSApp windows].
/// Returns null if unavailable.
unsafe fn get_app_window() -> *mut c_void {
    unsafe {
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

/// Create an autoreleased `NSString` from a Rust string.
unsafe fn ns_string(s: &str) -> *mut c_void {
    unsafe {
        let cstr = std::ffi::CString::new(s).unwrap_or_default();
        let cls = objc_getClass(c"NSString".as_ptr());
        let sel = sel_registerName(c"stringWithUTF8String:".as_ptr());
        let send: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        send(cls, sel, cstr.as_ptr())
    }
}

/// Show a simple "About" dialog with the app name, version, and author — matching
/// the Windows message box. (The standard macOS About panel does not surface the
/// author, so an NSAlert is used for parity.)
pub fn show_about() {
    unsafe {
        // Bring the Accessory app forward so the alert appears in front.
        let app_cls = objc_getClass(c"NSApplication".as_ptr());
        let app = objc_msgSend(app_cls, sel_registerName(c"sharedApplication".as_ptr()));
        if !app.is_null() {
            let msg_send_bool: MsgSendBool = std::mem::transmute(objc_msgSend as *const ());
            msg_send_bool(app, sel_registerName(c"activateIgnoringOtherApps:".as_ptr()), true);
        }

        let alert_cls = objc_getClass(c"NSAlert".as_ptr());
        if alert_cls.is_null() {
            return;
        }
        let alert = objc_msgSend(alert_cls, sel_registerName(c"alloc".as_ptr()));
        let alert = objc_msgSend(alert, sel_registerName(c"init".as_ptr()));
        if alert.is_null() {
            return;
        }

        let msg_send_ptr: MsgSendPtr = std::mem::transmute(objc_msgSend as *const ());
        let title = ns_string(concat!("clip-llm v", env!("CARGO_PKG_VERSION")));
        let body = ns_string(env!("CARGO_PKG_AUTHORS"));
        msg_send_ptr(alert, sel_registerName(c"setMessageText:".as_ptr()), title);
        msg_send_ptr(alert, sel_registerName(c"setInformativeText:".as_ptr()), body);

        // [alert runModal] — modal until the user dismisses it.
        objc_msgSend(alert, sel_registerName(c"runModal".as_ptr()));
    }
}

/// Show the overlay WITHOUT making it key or activating the app, pre-positioning
/// it synchronously like `show_and_focus_window`. The user's app stays key, so a
/// subsequent simulated Cmd+C still targets it — used during selection capture.
pub fn show_no_activate(position: Option<(f32, f32)>) {
    unsafe {
        let window = get_app_window();
        if window.is_null() {
            return;
        }

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
        // orderFront: shows the window without making it key or activating the app.
        msg_send_ptr(window, sel_registerName(c"orderFront:".as_ptr()), nil);
    }
}


unsafe extern "C" {
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
    // Prefer the NSScreen work area (excludes Dock and menu bar) so the
    // overlay is never clamped underneath the Dock (#36); fall back to the
    // full display bounds if the NSScreen lookup fails.
    work_area_at_point(x, y).or_else(|| full_display_bounds_at_point(x, y))
}

/// Full display bounds via CGDisplayBounds (includes Dock and menu bar).
fn full_display_bounds_at_point(x: f64, y: f64) -> Option<(f64, f64, f64, f64)> {
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

/// Convert an AppKit rect (global bottom-left origin, y-up) to CG global
/// coordinates (top-left origin, y-down). AppKit's global space is anchored
/// at the bottom-left of the primary screen, so the flip uses its height.
fn appkit_rect_to_cg(
    rect: (f64, f64, f64, f64),
    primary_height: f64,
) -> (f64, f64, f64, f64) {
    let (x, y, w, h) = rect;
    (x, primary_height - (y + h), w, h)
}

/// Half-open containment check: `[origin, origin + size)` on both axes.
fn bounds_contain(bounds: (f64, f64, f64, f64), x: f64, y: f64) -> bool {
    let (bx, by, w, h) = bounds;
    x >= bx && x < bx + w && y >= by && y < by + h
}

/// Work area (`NSScreen.visibleFrame`) of the screen containing the point,
/// converted to CG global coordinates to match `mouse_position()` and
/// `CGDisplayBounds`. Returns `None` when the lookup fails.
fn work_area_at_point(x: f64, y: f64) -> Option<(f64, f64, f64, f64)> {
    let msg_send_i64: MsgSendRetI64 = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    let msg_send_at: MsgSendUlongRetPtr =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe {
        let cls = objc_getClass(c"NSScreen".as_ptr());
        if cls.is_null() {
            return None;
        }
        let screens = objc_msgSend(cls, sel_registerName(c"screens".as_ptr()));
        if screens.is_null() {
            return None;
        }
        let count = msg_send_i64(screens, sel_registerName(c"count".as_ptr()));
        if count <= 0 {
            return None;
        }
        let at_index = sel_registerName(c"objectAtIndex:".as_ptr());
        let frame_sel = sel_registerName(c"frame".as_ptr());

        // Index 0 is the primary screen; its height anchors the AppKit→CG flip.
        let primary = msg_send_at(screens, at_index, 0);
        if primary.is_null() {
            return None;
        }
        let primary_height = msg_send_rect(primary, frame_sel).size.height;

        for i in 0..count {
            let screen = msg_send_at(screens, at_index, i as c_ulong);
            if screen.is_null() {
                continue;
            }
            let frame = msg_send_rect(screen, frame_sel);
            let frame_cg = appkit_rect_to_cg(
                (frame.origin.x, frame.origin.y, frame.size.width, frame.size.height),
                primary_height,
            );
            if bounds_contain(frame_cg, x, y) {
                let vis = msg_send_rect(screen, sel_registerName(c"visibleFrame".as_ptr()));
                return Some(appkit_rect_to_cg(
                    (vis.origin.x, vis.origin.y, vis.size.width, vis.size.height),
                    primary_height,
                ));
            }
        }
        None
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
    /// Simulate Cmd+C by posting CGEvent keyboard events. With a `target` pid
    /// the events go directly to that process (`CGEventPostToPid`), making the
    /// copy independent of which app is frontmost — the overlay taking focus
    /// (drag right after modifier release) or the user app-switching during
    /// the release wait no longer breaks the capture. Without a target, falls
    /// back to the HID tap (frontmost app). Requires Accessibility permission.
    fn simulate_copy(&self, target: Option<i32>) -> Result<(), PlatformError> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| PlatformError::CopyFailed("failed to create CGEventSource".into()))?;

        let key_down = CGEvent::new_keyboard_event(source.clone(), KEY_C, true)
            .map_err(|()| PlatformError::CopyFailed("failed to create key-down event".into()))?;
        key_down.set_flags(CGEventFlags::CGEventFlagCommand);

        let key_up = CGEvent::new_keyboard_event(source, KEY_C, false)
            .map_err(|()| PlatformError::CopyFailed("failed to create key-up event".into()))?;
        key_up.set_flags(CGEventFlags::CGEventFlagCommand);

        match target {
            Some(pid) => {
                debug!("posting Cmd+C key events to pid {pid}");
                key_down.post_to_pid(pid);
                thread::sleep(Duration::from_millis(KEY_EVENT_DELAY_MS));
                key_up.post_to_pid(pid);
            }
            None => {
                debug!("posting Cmd+C key events to HID");
                key_down.post(CGEventTapLocation::HID);
                thread::sleep(Duration::from_millis(KEY_EVENT_DELAY_MS));
                key_up.post(CGEventTapLocation::HID);
            }
        }

        Ok(())
    }

    /// Pid of the frontmost application via `NSWorkspace.frontmostApplication`.
    fn frontmost_app_pid(&self) -> Option<i32> {
        let msg_send_i32: MsgSendRetI32 =
            unsafe { std::mem::transmute(objc_msgSend as *const ()) };
        unsafe {
            let cls = objc_getClass(c"NSWorkspace".as_ptr());
            if cls.is_null() {
                return None;
            }
            let workspace = objc_msgSend(cls, sel_registerName(c"sharedWorkspace".as_ptr()));
            if workspace.is_null() {
                return None;
            }
            let app = objc_msgSend(workspace, sel_registerName(c"frontmostApplication".as_ptr()));
            if app.is_null() {
                return None;
            }
            Some(msg_send_i32(app, sel_registerName(c"processIdentifier".as_ptr())))
        }
    }

    /// Read `NSPasteboard.generalPasteboard.changeCount` — increments whenever
    /// any app takes pasteboard ownership (e.g. a copy lands).
    fn clipboard_change_count(&self) -> u64 {
        let msg_send_i64: MsgSendRetI64 =
            unsafe { std::mem::transmute(objc_msgSend as *const ()) };
        unsafe {
            let cls = objc_getClass(c"NSPasteboard".as_ptr());
            if cls.is_null() {
                return 0;
            }
            let pasteboard = objc_msgSend(cls, sel_registerName(c"generalPasteboard".as_ptr()));
            if pasteboard.is_null() {
                return 0;
            }
            msg_send_i64(pasteboard, sel_registerName(c"changeCount".as_ptr())) as u64
        }
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
        let trusted = ax_is_process_trusted_with_prompt();
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

    fn show_window_no_activate(&self, pos: Option<(f32, f32)>) -> bool {
        configure_window_for_spaces();
        show_no_activate(pos);
        false // no egui Visible(true) sync needed on macOS
    }

    fn hide_window(&self) -> bool {
        false // caller should use ViewportCommand::Visible(false)
    }

    fn reposition_window(&self, _x: f32, _y: f32) -> bool {
        false // caller should use ViewportCommand::OuterPosition
    }

    fn exclude_from_taskbar(&self) {
        // No-op: the Accessory activation policy already keeps the app out of
        // the Dock and Cmd+Tab, and macOS has no taskbar.
    }

    fn paste_to_foreground(&self) -> Result<(), PlatformError> {
        // Deactivate this app so the OS activates the previously focused app.
        // [NSApp hide:nil] is AppKit and must run on the main thread, so it stays
        // synchronous here; it is fast. The slow part — the WindowServer focus-
        // transfer wait plus key simulation (~150ms) — runs on a short-lived
        // background thread so the egui render loop is not frozen on every paste.
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
        // CGEvent posting is thread-safe, so the delayed paste is safe off-thread.
        // MacOsPlatform is a ZST, so construct a fresh one for the thread.
        thread::spawn(|| {
            // Wait for WindowServer focus transfer to complete before pasting.
            thread::sleep(Duration::from_millis(FOCUS_TRANSFER_DELAY_MS));
            if let Err(e) = MacOsPlatform.simulate_paste() {
                warn!("paste simulation failed: {e}");
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{appkit_rect_to_cg, bounds_contain};

    #[test]
    fn appkit_to_cg_flips_y_around_primary_height() {
        // Primary screen 2560x1440, menu bar 25pt, Dock 70pt at the bottom:
        // visibleFrame (AppKit, y-up) = origin y 70, height 1345.
        let cg = appkit_rect_to_cg((0.0, 70.0, 2560.0, 1345.0), 1440.0);
        // In CG (top-left origin) the work area starts below the menu bar.
        assert_eq!(cg, (0.0, 25.0, 2560.0, 1345.0));
    }

    #[test]
    fn appkit_to_cg_secondary_screen_above_primary() {
        // Secondary 1920x1080 sitting above the primary (AppKit y = 1440).
        let cg = appkit_rect_to_cg((100.0, 1440.0, 1920.0, 1080.0), 1440.0);
        // Above the primary in CG means negative y.
        assert_eq!(cg, (100.0, -1080.0, 1920.0, 1080.0));
    }

    #[test]
    fn bounds_contain_is_half_open() {
        let b = (0.0, 0.0, 100.0, 50.0);
        assert!(bounds_contain(b, 0.0, 0.0));
        assert!(bounds_contain(b, 99.9, 49.9));
        assert!(!bounds_contain(b, 100.0, 0.0));
        assert!(!bounds_contain(b, 0.0, 50.0));
        assert!(!bounds_contain(b, -0.1, 0.0));
    }
}
