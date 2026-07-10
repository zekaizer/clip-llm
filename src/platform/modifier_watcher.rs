//! Listen-only OS keyboard watcher tracking whether the cycle modifiers
//! (Ctrl AND Shift) are currently held.
//!
//! `global-hotkey` only fires for the exact `Ctrl+Shift+C` chord and cannot
//! observe a bare modifier release, which is the "commit" signal for the
//! hold-to-cycle mode picker. This watcher fills that gap with a passive,
//! listen-only hook that never consumes or alters events:
//! - macOS: a `CGEventTap` for `FlagsChanged` only, run on a dedicated thread
//!   with its own `CFRunLoop`. It reads `CGEventGetFlags` exclusively — it does
//!   NOT touch Text Input Services, so it is safe off the main thread.
//! - Windows: a `WH_KEYBOARD_LL` hook on a dedicated message-pump thread.
//!
//! State is exposed as a shared lock-free flag rather than an event stream, so
//! the consumer never has to drain an unbounded backlog of modifier events that
//! pile up while it is otherwise idle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shared, lock-free view of whether the cycle modifiers (Ctrl AND Shift) are
/// both physically held right now. Cloning shares the same underlying flag.
///
/// A watcher that fails to install (e.g. missing permission, unsupported OS)
/// leaves this permanently `false`, so mode cycling simply stays unavailable
/// while the existing hotkey trigger keeps working. `is_active()` lets other
/// consumers (e.g. `ClipboardManager::copy_and_read`'s modifier-release wait)
/// distinguish that degraded "always false" state from a genuine observed
/// release, since both otherwise look identical through `combo_held()` alone.
#[derive(Clone, Default)]
pub struct ModifierState {
    held: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
}

impl ModifierState {
    fn new() -> Self {
        Self::default()
    }

    /// Whether Ctrl and Shift are both held at this instant.
    pub fn combo_held(&self) -> bool {
        self.held.load(Ordering::Acquire)
    }

    /// Whether a real OS watcher is installed and feeding this state. `false`
    /// means `combo_held()` is a permanently-unset default (failed install or
    /// unsupported platform), not a live signal.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Update from the raw modifier flags (called by the watcher thread).
    fn set(&self, ctrl: bool, shift: bool) {
        self.held.store(ctrl && shift, Ordering::Release);
    }

    /// Mark the watcher as successfully installed and live (called once by
    /// the watcher thread right after it starts receiving real OS events).
    fn set_active(&self) {
        self.active.store(true, Ordering::Release);
    }

    /// Test-only: directly drive the combo-held state to simulate the OS watcher.
    #[cfg(test)]
    pub(crate) fn set_combo_held(&self, held: bool) {
        self.held.store(held, Ordering::Release);
    }

    /// Test-only: directly drive the active flag to simulate a live watcher.
    #[cfg(test)]
    pub(crate) fn set_active_for_test(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }
}

/// Spawn the platform keyboard watcher and return the shared modifier state.
///
/// The watcher runs for the process lifetime on its own thread. On failure it
/// logs a warning and returns a state that stays `false` (graceful degrade).
#[cfg(target_os = "macos")]
pub fn spawn_modifier_watcher() -> ModifierState {
    let state = ModifierState::new();
    let state_thread = state.clone();
    if let Err(e) = std::thread::Builder::new()
        .name("modifier-watcher".into())
        .spawn(move || mac::run(state_thread))
    {
        tracing::warn!("failed to spawn modifier-watcher thread: {e}; mode cycling disabled");
    }
    state
}

#[cfg(target_os = "windows")]
pub fn spawn_modifier_watcher() -> ModifierState {
    let state = ModifierState::new();
    let state_thread = state.clone();
    if let Err(e) = std::thread::Builder::new()
        .name("modifier-watcher".into())
        .spawn(move || win::run(state_thread))
    {
        tracing::warn!("failed to spawn modifier-watcher thread: {e}; mode cycling disabled");
    }
    state
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn spawn_modifier_watcher() -> ModifierState {
    // No watcher on this platform — cycling stays unavailable.
    ModifierState::new()
}

#[cfg(target_os = "macos")]
mod mac {
    use super::ModifierState;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use core_foundation::runloop::{
        kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopRunResult,
    };
    use core_graphics::event::{
        CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
        CGEventType, CallbackResult,
    };
    use tracing::{info, warn};

    /// Re-enable poll interval; also bounds how long the thread sleeps if the
    /// run loop ever returns with no sources.
    const TICK: Duration = Duration::from_millis(250);

    pub(super) fn run(state: ModifierState) {
        // Set by the callback when macOS disables the tap (slow callback or
        // user-input timeout); the loop below re-enables it.
        let re_enable = Arc::new(AtomicBool::new(false));
        let re_enable_cb = re_enable.clone();
        let state_cb = state.clone();

        let tap = match CGEventTap::new(
            CGEventTapLocation::HID,
            CGEventTapPlacement::TailAppendEventTap,
            CGEventTapOptions::ListenOnly,
            vec![CGEventType::FlagsChanged],
            move |_proxy, etype, event| {
                match etype {
                    CGEventType::FlagsChanged => {
                        let flags = event.get_flags();
                        state_cb.set(
                            flags.contains(CGEventFlags::CGEventFlagControl),
                            flags.contains(CGEventFlags::CGEventFlagShift),
                        );
                    }
                    CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                        re_enable_cb.store(true, Ordering::Release);
                    }
                    _ => {}
                }
                // Listen-only: always pass the event through unchanged.
                CallbackResult::Keep
            },
        ) {
            Ok(tap) => tap,
            Err(()) => {
                warn!("CGEventTap creation failed (accessibility permission?); mode cycling disabled");
                return;
            }
        };

        let source = match tap.mach_port().create_runloop_source(0) {
            Ok(s) => s,
            Err(()) => {
                warn!("CGEventTap run-loop source creation failed; mode cycling disabled");
                return;
            }
        };

        let runloop = CFRunLoop::get_current();
        runloop.add_source(&source, unsafe { kCFRunLoopCommonModes });
        tap.enable();
        state.set_active();
        info!("modifier watcher active (CGEventTap, FlagsChanged, listen-only)");

        // `tap` and `source` must stay alive for the run loop's lifetime — the
        // tap is invalidated on drop. This loop never returns (process-lifetime).
        loop {
            let result =
                CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, TICK, false);
            if re_enable.swap(false, Ordering::AcqRel) {
                tap.enable();
                warn!("re-enabled modifier-watch CGEventTap after OS disable");
            }
            // Defensive: if the mode ever drains with no sources, avoid a hot spin.
            if result == CFRunLoopRunResult::Finished {
                std::thread::sleep(TICK);
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod win {
    use super::ModifierState;
    use std::cell::{Cell, RefCell};

    use tracing::{info, warn};
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        VK_CONTROL, VK_LCONTROL, VK_RCONTROL, VK_LSHIFT, VK_RSHIFT, VK_SHIFT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage,
        UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYUP, WM_SYSKEYUP,
    };

    thread_local! {
        static STATE: RefCell<Option<ModifierState>> = const { RefCell::new(None) };
        static CTRL_DOWN: Cell<bool> = const { Cell::new(false) };
        static SHIFT_DOWN: Cell<bool> = const { Cell::new(false) };
    }

    /// Low-level keyboard hook callback. Runs on the watcher thread (where the
    /// hook was installed). Listen-only: always forwards via `CallNextHookEx`.
    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            let vk = kb.vkCode;
            let msg = wparam as u32;
            let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            let is_ctrl =
                vk == VK_CONTROL as u32 || vk == VK_LCONTROL as u32 || vk == VK_RCONTROL as u32;
            let is_shift =
                vk == VK_SHIFT as u32 || vk == VK_LSHIFT as u32 || vk == VK_RSHIFT as u32;
            if is_ctrl || is_shift {
                if is_ctrl {
                    CTRL_DOWN.with(|c| c.set(!is_up));
                }
                if is_shift {
                    SHIFT_DOWN.with(|s| s.set(!is_up));
                }
                let ctrl = CTRL_DOWN.with(|c| c.get());
                let shift = SHIFT_DOWN.with(|s| s.get());
                STATE.with(|s| {
                    if let Some(st) = s.borrow().as_ref() {
                        st.set(ctrl, shift);
                    }
                });
            }
        }
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }

    pub(super) fn run(state: ModifierState) {
        STATE.with(|s| *s.borrow_mut() = Some(state.clone()));

        // hmod = null is valid for WH_KEYBOARD_LL on modern Windows; dwthreadid
        // = 0 installs a global (all-threads) hook.
        let hook: HHOOK =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), std::ptr::null_mut(), 0) };
        if hook.is_null() {
            warn!("SetWindowsHookExW(WH_KEYBOARD_LL) failed; mode cycling disabled");
            STATE.with(|s| *s.borrow_mut() = None);
            return;
        }
        state.set_active();
        info!("modifier watcher active (WH_KEYBOARD_LL, listen-only)");

        // The LL hook is delivered through this thread's message queue, so the
        // thread must pump messages for the callback to fire.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        loop {
            let ret = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
            if ret <= 0 {
                // 0 = WM_QUIT, -1 = error.
                break;
            }
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        unsafe { UnhookWindowsHookEx(hook) };
        STATE.with(|s| *s.borrow_mut() = None);
    }
}
