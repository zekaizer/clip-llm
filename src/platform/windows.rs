use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::thread;
use std::time::Duration;

use tracing::{debug, warn};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_C, VK_CONTROL,
    VK_SHIFT, VK_V,
};
use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::UI::HiDpi::GetDpiForSystem;
use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

use super::Platform;
use crate::PlatformError;

/// Delay between key events (ms).
const KEY_EVENT_DELAY_MS: u64 = 50;
/// Delay for OS focus transfer after SW_HIDE (ms).
const FOCUS_TRANSFER_DELAY_MS: u64 = 100;

pub struct WindowsPlatform;

impl WindowsPlatform {
    fn simulate_paste(&self) -> Result<(), PlatformError> {
        debug!("posting Ctrl+V key events via SendInput");

        let inputs = [
            // Release any held modifier keys.
            make_key_input(VK_SHIFT, KEYEVENTF_KEYUP),
            make_key_input(VK_CONTROL, KEYEVENTF_KEYUP),
            // Clean Ctrl+V
            make_key_input(VK_CONTROL, 0),
            make_key_input(VK_V, 0),
            make_key_input(VK_V, KEYEVENTF_KEYUP),
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
            return Err(PlatformError::PasteFailed(format!(
                "SendInput returned {sent}, expected {}",
                inputs.len()
            )));
        }

        thread::sleep(Duration::from_millis(KEY_EVENT_DELAY_MS));
        Ok(())
    }
}

impl Platform for WindowsPlatform {
    /// Simulate Ctrl+C by sending keyboard input via SendInput.
    /// `target` is ignored: SendInput always goes to the foreground window —
    /// Windows has no public per-process key injection (#56 keeps the overlay
    /// from taking the foreground instead).
    fn simulate_copy(&self, _target: Option<i32>) -> Result<(), PlatformError> {
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

    /// No per-process key injection on Windows; see `simulate_copy`.
    fn frontmost_app_pid(&self) -> Option<i32> {
        None
    }

    /// Read the global clipboard sequence number — increments whenever the
    /// clipboard contents change (e.g. a copy lands).
    fn clipboard_change_count(&self) -> u64 {
        use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;
        u64::from(unsafe { GetClipboardSequenceNumber() })
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
        // GetCursorPos returns physical pixels in DPI-aware processes. Convert
        // to logical points using the DPI of the monitor the cursor is
        // actually on (not the primary monitor's system DPI), so multi-monitor
        // setups with mixed DPI (e.g. primary 100% + secondary 150%) stay correct.
        let scale = dpi_scale_at_physical_point(pt);
        Some((pt.x as f64 / scale, pt.y as f64 / scale))
    }

    fn display_bounds_at_point(&self, x: f64, y: f64) -> Option<(f64, f64, f64, f64)> {
        use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
        let (hmon, scale) = resolve_logical_point(x, y);
        if hmon.is_null() {
            return None;
        }
        let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if unsafe { GetMonitorInfoW(hmon, &mut info) } == 0 {
            return None;
        }
        // rcWork = work area (excludes taskbar). Convert back to logical points
        // using the same monitor's DPI.
        let rc = info.rcWork;
        Some((
            rc.left as f64 / scale,
            rc.top as f64 / scale,
            (rc.right - rc.left) as f64 / scale,
            (rc.bottom - rc.top) as f64 / scale,
        ))
    }

    fn show_window(&self, pos: Option<(f32, f32)>) -> bool {
        show_and_focus_window(pos);
        true // needs Visible(true) to sync winit state (ControlFlow::Wait, egui#5229)
    }

    fn show_window_no_activate(&self, pos: Option<(f32, f32)>) -> bool {
        show_no_activate_at(pos);
        // SW_SHOWNA delivers WM_PAINT and keeps ControlFlow::Wait without a
        // Visible(true) sync, so we must NOT request one here (it would steal focus).
        false
    }

    fn hide_window(&self) -> bool {
        move_window_offscreen();
        true // handled natively; caller must NOT send Visible(false)
    }

    fn reposition_window(&self, x: f32, y: f32) -> bool {
        set_window_position(x, y);
        true // handled natively; caller must NOT send OuterPosition
    }

    fn exclude_from_taskbar(&self) {
        set_tool_window();
    }

    fn paste_to_foreground(&self) -> Result<(), PlatformError> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

        // SW_HIDE transfers foreground to the previous app. move_window_offscreen()
        // alone keeps us as foreground, so SendInput would target the overlay.
        // Done synchronously on the calling thread (fast).
        if let Some(h) = clip_llm_hwnd() {
            unsafe { ShowWindow(h, SW_HIDE); }
            debug!("yielded focus via ShowWindow(SW_HIDE)");
        } else {
            warn!("paste_to_foreground: window not found, paste may target wrong app");
        }

        // The focus-transfer wait + key simulation + visibility restore (~150ms)
        // run on a short-lived background thread so the egui render loop is not
        // frozen on every paste. HWND is !Send, so it cannot be captured by the
        // closure directly; re-resolve it inside the thread via `clip_llm_hwnd()`
        // (backed by the cross-thread-safe `CACHED_HWND` atomic, so this is cheap
        // rather than a fresh `FindWindowW`). WindowsPlatform is a ZST.
        thread::spawn(|| {
            use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindowAsync, SW_SHOWNA};
            // Wait for the target app to become foreground and ready for input.
            thread::sleep(Duration::from_millis(FOCUS_TRANSFER_DELAY_MS));
            let result = WindowsPlatform.simulate_paste();
            // Restore offscreen-but-visible state to avoid eframe CPU spin
            // (egui#5229): SW_HIDE triggers ControlFlow::Poll; SW_SHOWNA at
            // (-32000,-32000) restores Wait. Do NOT use show_no_activate() — it
            // repositions to cursor.
            if let Some(h) = clip_llm_hwnd() {
                unsafe { ShowWindowAsync(h, SW_SHOWNA); }
            }
            if let Err(e) = result {
                warn!("paste simulation failed: {e}");
            }
        });
        Ok(())
    }
}

/// Cached handle to the clip-llm overlay window, stored as a pointer-sized
/// integer. `HWND` is a raw pointer (`!Send`/`!Sync`), so it cannot be held in
/// a `static` directly; `isize` can. `0` means "not yet resolved" (Win32 never
/// hands out a null HWND for a real window).
static CACHED_HWND: AtomicIsize = AtomicIsize::new(0);

/// Tracks whether the last fallback lookup failed, so the failure is logged
/// only on the found -> not-found transition instead of every call. Several
/// call sites (notably `set_window_position`, invoked once per frame via
/// `reposition_window` while the overlay is visible) would otherwise spam the
/// log if the window were ever briefly unresolvable.
static HWND_LOOKUP_FAILED: AtomicBool = AtomicBool::new(false);

/// Resolve the clip-llm overlay `HWND`, preferring a cached handle over the
/// system-wide `FindWindowW` title lookup.
///
/// `FindWindowW` enumerates top-level windows and is too costly to call from
/// hot paths — several call sites run every frame while the overlay is
/// visible (`reposition_window` -> `set_window_position`). The window handle
/// is stable for the lifetime of the overlay window once created, so it is
/// cached in `CACHED_HWND` and revalidated on each call with `IsWindow`
/// (a single, cheap kernel call) rather than re-searched. A cache miss (unset
/// or stale, e.g. the window was destroyed and recreated) falls back to the
/// original `FindWindowW` lookup, which repopulates the cache on success.
fn clip_llm_hwnd() -> Option<HWND> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, IsWindow};

    let cached = CACHED_HWND.load(Ordering::Relaxed);
    if cached != 0 {
        let hwnd = cached as HWND;
        if unsafe { IsWindow(hwnd) } != 0 {
            HWND_LOOKUP_FAILED.store(false, Ordering::Relaxed);
            return Some(hwnd);
        }
        // Stale handle (window destroyed, possibly since recreated) — drop it
        // and fall through to a fresh lookup below.
        CACHED_HWND.store(0, Ordering::Relaxed);
    }

    let title: Vec<u16> = "clip-llm\0".encode_utf16().collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() {
        // Log only on the found -> not-found transition to avoid spamming
        // per-frame call sites; this also keeps quiet during the brief
        // startup window before the overlay is created.
        if !HWND_LOOKUP_FAILED.swap(true, Ordering::Relaxed) {
            warn!("clip_llm_hwnd: FindWindowW could not locate the overlay window");
        }
        None
    } else {
        HWND_LOOKUP_FAILED.store(false, Ordering::Relaxed);
        CACHED_HWND.store(hwnd as isize, Ordering::Relaxed);
        Some(hwnd)
    }
}

/// Add or remove `WS_EX_NOACTIVATE` on the overlay window.
///
/// While set, mouse clicks (including drag, which winit performs via
/// `WM_NCLBUTTONDOWN`) no longer make the overlay the foreground window, so a
/// click during the capture window cannot redirect `SendInput(Ctrl+C)` away
/// from the source app (#56). Cleared again before the overlay is shown with
/// focus so the user can interact with the Result state normally.
fn set_no_activate(hwnd: HWND, enabled: bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE,
    };
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_style = if enabled {
            style | WS_EX_NOACTIVATE as isize
        } else {
            style & !(WS_EX_NOACTIVATE as isize)
        };
        if new_style != style {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style);
        }
    }
}

/// Keep the overlay out of the taskbar by clearing `WS_EX_APPWINDOW` and setting
/// `WS_EX_TOOLWINDOW`.
///
/// The shell lists a window in the taskbar whenever `WS_EX_APPWINDOW` is present,
/// *regardless* of `WS_EX_TOOLWINDOW` — APPWINDOW wins. winit forces APPWINDOW
/// onto every unowned top-level window (its `ON_TASKBAR` flag, set in
/// `window.rs` when the window has no owner) and never sets `WS_EX_TOOLWINDOW`,
/// so merely adding TOOLWINDOW is not enough; APPWINDOW must be cleared too.
/// winit's `with_taskbar(false)` only issues a one-shot `ITaskbarList::DeleteTab`,
/// which the shell undoes on its next re-evaluation, so the style bits must be
/// changed directly.
///
/// Timing is critical. winit rebuilds the *entire* exstyle from its own
/// `WindowFlags` — re-adding APPWINDOW, dropping TOOLWINDOW — on every transition
/// of its internal `VISIBLE` flag (`WindowState::apply_diff`). So this must run
/// *after* winit's last such recomputation, or the bits are wiped. The earlier
/// "set once on the first frame, then it's permanent" approach was wrong: the
/// first focus-show flips `VISIBLE` false->true and wiped the exclusion.
/// `maybe_initial_hide` now drives that one transition at startup while the
/// window is offscreen and calls this on the following frame; the app never
/// toggles winit visibility again, so the exclusion sticks for good.
fn set_tool_window() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, IsWindowVisible, SetWindowLongPtrW, SetWindowPos, ShowWindow,
        GWL_EXSTYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        SW_HIDE, SW_SHOWNA, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };
    if let Some(hwnd) = clip_llm_hwnd() {
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let new_style = (style | WS_EX_TOOLWINDOW as isize) & !(WS_EX_APPWINDOW as isize);
            if new_style == style {
                return;
            }
            let was_visible = IsWindowVisible(hwnd) != 0;
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style);
            // SetWindowLong style changes require SWP_FRAMECHANGED to take effect.
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
            // The taskbar re-reads APPWINDOW/TOOLWINDOW only on a hide -> show
            // transition, not on SWP_FRAMECHANGED. The window is offscreen but
            // mapped when this runs (maybe_initial_hide synced winit's VISIBLE
            // flag the previous frame), so cycle it to force the re-read; the
            // cycle is invisible offscreen. SW_SHOWNA keeps position and does not
            // activate. The was_visible guard stays as a safety net.
            if was_visible {
                ShowWindow(hwnd, SW_HIDE);
                ShowWindow(hwnd, SW_SHOWNA);
            }
        }
    }
}

/// DPI scale factor derived from the system primary DPI setting (physical pixels / logical points).
///
/// Only used as an initial guess before a monitor is resolved (see `resolve_logical_point`)
/// and as a fallback when a per-monitor DPI query fails. Do NOT use this directly for
/// coordinate conversion on multi-monitor setups — the primary monitor's DPI does not apply
/// to secondary monitors with a different scale factor.
fn system_dpi_scale() -> f64 {
    let dpi = unsafe { GetDpiForSystem() } as f64;
    dpi / 96.0
}

/// DPI scale (physical pixels / logical points) for a specific monitor, via `GetDpiForMonitor`.
/// Falls back to `system_dpi_scale()` if the handle is null or the query fails.
fn monitor_dpi_scale(hmon: windows_sys::Win32::Graphics::Gdi::HMONITOR) -> f64 {
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    if hmon.is_null() {
        return system_dpi_scale();
    }
    let mut dpi_x = 0u32;
    let mut dpi_y = 0u32;
    if unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) } == 0 {
        dpi_x as f64 / 96.0
    } else {
        system_dpi_scale()
    }
}

/// DPI scale for the monitor containing an unambiguous physical-pixel point
/// (e.g. straight from `GetCursorPos`).
fn dpi_scale_at_physical_point(pt: POINT) -> f64 {
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
    monitor_dpi_scale(unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) })
}

/// Resolve the monitor and DPI scale for a logical point `(x, y)` whose originating scale is
/// not known ahead of time (e.g. a cursor position or window-target position computed
/// elsewhere, which may have used a different monitor's DPI than the one it actually falls
/// on near a monitor boundary).
///
/// Two-pass: the system DPI converts the logical point to an approximate physical point to
/// locate a candidate monitor, then that monitor's real DPI re-derives the physical point and
/// re-resolves the monitor. This is exact except in the rare case where the refined point
/// crosses onto yet another monitor, which is not worth chasing further here.
fn resolve_logical_point(x: f64, y: f64) -> (windows_sys::Win32::Graphics::Gdi::HMONITOR, f64) {
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
    let approx_scale = system_dpi_scale();
    let approx_pt = POINT {
        x: (x * approx_scale) as i32,
        y: (y * approx_scale) as i32,
    };
    let candidate_scale = dpi_scale_at_physical_point(approx_pt);
    let pt = POINT {
        x: (x * candidate_scale) as i32,
        y: (y * candidate_scale) as i32,
    };
    let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    (hmon, monitor_dpi_scale(hmon))
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
        GetWindowRect, SetWindowPos, ShowWindowAsync, HWND_TOP, SW_SHOWNA,
        SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };
    if let Some(hwnd) = clip_llm_hwnd() {
        set_no_activate(hwnd, true);
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

/// Show a simple About message box with the app name, version, and author.
///
/// `MessageBoxW` pumps its own modal message loop; starting one inside the
/// winit event-loop handler hangs the app (nested modal loop, winit#1698),
/// so the box runs on a dedicated thread. `MB_TOPMOST` keeps it above the
/// always-on-top overlay window; `MB_SETFOREGROUND` brings it to front.
pub fn show_about() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };

    // The box no longer blocks the caller, so repeated menu clicks would
    // stack dialogs without this guard.
    static ABOUT_OPEN: AtomicBool = AtomicBool::new(false);
    if ABOUT_OPEN.swap(true, Ordering::SeqCst) {
        return;
    }

    std::thread::spawn(|| {
        let body: Vec<u16> = format!(
            "clip-llm v{}\n{}",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_AUTHORS")
        )
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
        let title: Vec<u16> =
            "About clip-llm".encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let _ = MessageBoxW(
                std::ptr::null_mut(),
                body.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONINFORMATION | MB_TOPMOST | MB_SETFOREGROUND,
            );
        }
        ABOUT_OPEN.store(false, Ordering::SeqCst);
    });
}

/// Show a native, modal message box with the given title and message, blocking
/// the calling thread until the user dismisses it.
///
/// Unlike [`show_about`], this runs `MessageBoxW` directly on the calling thread
/// rather than a dedicated one — safe here because callers use this only for
/// fatal startup errors, before the winit event loop starts (so the nested
/// modal loop hazard that `show_about` works around, winit#1698, does not
/// apply). Blocking is desirable: it keeps the process alive long enough for
/// the user to read the message before the caller exits.
pub fn show_alert_blocking(title: &str, message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    };

    let body: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let _ = MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR | MB_TOPMOST | MB_SETFOREGROUND,
        );
    }
}

/// Show the clip-llm window at `position` (logical points) WITHOUT activating it,
/// so the foreground app stays the same and `SendInput(Ctrl+C)` still targets it.
/// Like `show_no_activate()` but honors an explicit position (the capture spawn
/// point) instead of re-centering on the cursor.
pub fn show_no_activate_at(position: Option<(f32, f32)>) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, ShowWindowAsync, HWND_TOP, SW_SHOWNA, SWP_NOACTIVATE, SWP_NOSIZE,
        SWP_NOZORDER,
    };
    if let Some(hwnd) = clip_llm_hwnd() {
        set_no_activate(hwnd, true);
        unsafe {
            if let Some((x, y)) = position {
                // Target monitor for this position, not the primary monitor.
                let (_, scale) = resolve_logical_point(x as f64, y as f64);
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
        SetForegroundWindow, SetWindowPos, ShowWindowAsync, HWND_TOP, SW_SHOW,
        SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };
    if let Some(hwnd) = clip_llm_hwnd() {
        // The capture is over once the overlay is shown with focus — restore
        // normal click-to-activate behavior for the Result/Error states.
        set_no_activate(hwnd, false);
        unsafe {
            if let Some((x, y)) = position {
                // Target monitor for this position, not the primary monitor.
                let (_, scale) = resolve_logical_point(x as f64, y as f64);
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

/// Move the clip-llm window to the given position without showing or focusing it.
///
/// Coordinates are logical points in the space produced by `mouse_position()` /
/// `display_bounds_at_point()`: physical pixels divided by the DPI of whichever monitor the
/// point actually falls on (see `resolve_logical_point`), not a single global system DPI —
/// mixed-DPI multi-monitor setups (e.g. primary 100% + secondary 150%) need the target
/// monitor's own scale to land in the right place.
///
/// Uses `SetWindowPos` directly to bypass winit's per-monitor DPI scaling, which would
/// otherwise mis-scale the coordinates when the window is on a secondary monitor with a
/// different DPI.
pub fn set_window_position(x: f32, y: f32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOP, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };
    if let Some(hwnd) = clip_llm_hwnd() {
        let (_, scale) = resolve_logical_point(x as f64, y as f64);
        unsafe {
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
    }
}

/// Move the clip-llm window off-screen instead of hiding it.
///
/// Bypasses eframe's `Visible(false)` which triggers `ControlFlow::Poll` and
/// ~10% CPU spin (egui#5229). The window stays visible from winit's perspective,
/// so `WM_PAINT` is still delivered and repaint entries are consumed normally,
/// keeping `ControlFlow::Wait` (zero CPU).
pub fn move_window_offscreen() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    };
    if let Some(hwnd) = clip_llm_hwnd() {
        unsafe {
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                -32000,
                -32000,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
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
