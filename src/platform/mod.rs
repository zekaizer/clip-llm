/// Platform abstraction trait for OS-specific operations.
pub trait Platform {
    /// Simulate Cmd+C (macOS) or Ctrl+C (Windows) to copy selected text.
    ///
    /// `target` is the frontmost-app pid recorded at capture-trigger time
    /// (see [`Platform::frontmost_app_pid`]). When `Some`, macOS posts the
    /// key events directly to that process, so the copy still reaches the
    /// source app even if the overlay stole focus meanwhile (e.g. the user
    /// grabbed it to drag right after releasing the modifiers). Windows
    /// ignores it — `SendInput` cannot target a process.
    fn simulate_copy(&self, target: Option<i32>) -> Result<(), crate::PlatformError>;

    /// Process id of the frontmost application, recorded at capture-trigger
    /// time and later passed to [`Platform::simulate_copy`]. Returns `None`
    /// on platforms without per-process key-event posting (Windows).
    fn frontmost_app_pid(&self) -> Option<i32>;

    /// Monotonic clipboard change counter (macOS `NSPasteboard.changeCount`,
    /// Windows `GetClipboardSequenceNumber`). Bumps whenever any app takes
    /// clipboard ownership, letting callers detect that a simulated copy has
    /// landed without clearing the clipboard first.
    fn clipboard_change_count(&self) -> u64;

    /// Check and prompt for required OS permissions (e.g. macOS Accessibility).
    fn check_accessibility(&self) -> Result<(), crate::PlatformError>;

    /// Get the current mouse cursor position in **platform screen coordinates**:
    /// Quartz logical points on macOS, physical virtual-screen pixels on
    /// Windows. This is the single coordinate space shared by
    /// [`Platform::display_bounds_at_point`], [`Platform::show_window`],
    /// [`Platform::show_window_no_activate`] and [`Platform::reposition_window`].
    ///
    /// Windows deliberately does NOT divide by any DPI scale: with mixed
    /// per-monitor DPI the "physical ÷ monitor scale" mapping is not
    /// invertible (a point on a 150% secondary monitor collides with a point
    /// on a 100% primary one), so positions stay physical end-to-end and only
    /// *sizes* are converted via [`Platform::points_to_screen_scale_at`].
    fn mouse_position(&self) -> Option<(f64, f64)>;

    /// Get the display work area of the monitor containing the given point,
    /// in the same platform screen coordinates as [`Platform::mouse_position`].
    /// Returns (origin_x, origin_y, width, height). Work area excludes taskbar/dock.
    fn display_bounds_at_point(&self, x: f64, y: f64) -> Option<(f64, f64, f64, f64)>;

    /// Scale factor converting egui logical points to platform screen units at
    /// the given screen-space point: the containing monitor's DPI scale
    /// (physical pixels per point) on Windows; 1.0 on macOS, whose screen
    /// space (Quartz) is already measured in points.
    fn points_to_screen_scale_at(&self, _x: f64, _y: f64) -> f64 {
        1.0
    }

    /// Show and focus the overlay window at an optional position.
    /// Returns true if an egui `Visible(true)` viewport sync is also needed
    /// (Windows winit workaround to maintain ControlFlow::Wait, egui#5229).
    fn show_window(&self, pos: Option<(f32, f32)>) -> bool;

    /// Show the overlay at an optional position WITHOUT taking keyboard focus, so
    /// the user's app stays key and a simulated Cmd+C/Ctrl+C still targets it.
    /// Used while capturing the current selection on double-tap.
    /// Returns true if an egui `Visible(true)` viewport sync is also needed.
    fn show_window_no_activate(&self, pos: Option<(f32, f32)>) -> bool;

    /// Hide the overlay window. Returns true if handled natively (caller must not send
    /// `Visible(false)`); false means the caller should send `ViewportCommand::Visible(false)`.
    fn hide_window(&self) -> bool;

    /// Reposition the window using a direct native API call.
    /// Returns true if handled natively (caller must not send `OuterPosition`).
    fn reposition_window(&self, x: f32, y: f32) -> bool;

    /// Paste clipboard content into the previously focused application.
    /// Handles focus transfer, timing, key simulation, and platform-specific cleanup.
    fn paste_to_foreground(&self) -> Result<(), crate::PlatformError>;

    /// Permanently exclude the overlay window from the taskbar / app switcher.
    ///
    /// On Windows this clears `WS_EX_APPWINDOW` (which forces taskbar presence
    /// regardless of `WS_EX_TOOLWINDOW`) and sets `WS_EX_TOOLWINDOW` — a direct
    /// style change, unlike the one-shot `ITaskbarList::DeleteTab` winit uses for
    /// `with_taskbar(false)`, which the shell re-adds on its next re-evaluation.
    /// The style change is only durable if applied *after* winit's last exstyle
    /// recomputation, so `maybe_initial_hide` calls this on the frame following
    /// the startup `VISIBLE` sync (see its docs). No-op on macOS (the Accessory
    /// activation policy already excludes it).
    fn exclude_from_taskbar(&self);

    /// Check whether the app is currently configured to launch automatically at
    /// login. macOS: whether the LaunchAgent plist exists. Windows: whether the
    /// `Run` registry value exists. Never panics; returns `false` on any lookup
    /// failure (treated as "not enabled").
    fn launch_at_login_enabled(&self) -> bool;

    /// Enable or disable launching the app automatically at login.
    ///
    /// Enabling always (re)writes the launch entry to point at the current
    /// executable, so a stale entry left over from a moved/reinstalled binary
    /// is corrected rather than left pointing at the old path. Disabling
    /// removes the entry; it is not an error to disable when already disabled.
    fn set_launch_at_login(&self, enabled: bool) -> Result<(), crate::PlatformError>;
}

#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacOsPlatform as NativePlatform;

#[cfg(target_os = "windows")]
pub(crate) mod windows;
#[cfg(target_os = "windows")]
pub use windows::WindowsPlatform as NativePlatform;

// Listen-only keyboard watcher for the cycle-modifier (Ctrl+Shift) release
// signal. Cross-platform; cfg-gated internally.
pub mod modifier_watcher;
pub use modifier_watcher::{spawn_modifier_watcher, ModifierState};

// -- System tray (Windows taskbar / macOS menu bar) --
//
// `tray-icon` is cross-platform, so the implementation is shared here; only the
// Windows-specific window nudge in the menu handler is `cfg`-gated.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static TRAY_QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
/// "Reload Config" clicked; consumed by `poll_tray_events` on the main thread,
/// which owns the config swap and the worker notification.
static TRAY_RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);
/// "Settings…" clicked; the panel is opened by the UI on the main thread.
static TRAY_SETTINGS_REQUESTED: AtomicBool = AtomicBool::new(false);

/// A tray menu action the main thread must carry out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// A model profile was picked in the "Model" submenu (pool index).
    SelectModel(usize),
    /// "Reload Config" was clicked.
    ReloadConfig,
    /// "Settings…" was clicked.
    OpenSettings,
}
/// Model profile index picked in the tray "Model" submenu, consumed by
/// `poll_tray_events` on the main thread. `NO_MODEL_SELECTED` = nothing pending.
static TRAY_MODEL_SELECTED: AtomicUsize = AtomicUsize::new(NO_MODEL_SELECTED);
const NO_MODEL_SELECTED: usize = usize::MAX;

/// One entry of the tray "Model" submenu. Selectable entries come first, in
/// worker pool order (their position is the pool index); entries with
/// `unavailable = Some(reason)` are shown disabled after them.
#[derive(Debug, Clone, PartialEq)]
pub struct TrayModel {
    pub label: String,
    pub unavailable: Option<String>,
}
#[cfg(target_os = "macos")]
static TRAY_ABOUT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Mirrors the on-disk/registry launch-at-login state, updated whenever a
/// toggle attempt (successfully) changes it. Read from the menu event handler
/// thread to determine what a click intended to switch *to* — the native
/// checkbox has already auto-toggled its own visual by the time the event
/// fires (both muda backends flip `checked` before sending `MenuEvent`), so
/// this is the only way to know the target state without touching the
/// `Rc`-based menu item off the main thread.
static TRAY_LAUNCH_AT_LOGIN_STATE: AtomicBool = AtomicBool::new(false);

/// Set when a `set_launch_at_login` call from the menu event handler fails, so
/// `poll_tray_events` (main thread) can revert the checkbox to match reality.
/// Needed because the checkbox already shows the attempted (failed) state by
/// the time the handler runs — see `TRAY_LAUNCH_AT_LOGIN_STATE`.
static TRAY_LAUNCH_AT_LOGIN_REVERT: AtomicBool = AtomicBool::new(false);

/// Handles to the dynamic rows of the tray "Status" submenu, updated after the
/// startup probe lands and on every request outcome. `tray-icon` menu items are
/// `Rc`-based (not `Send`), so they live in a thread-local and may only be
/// touched from the main thread — which is where `init_tray` and the
/// `update_tray_*` callers (inside `OverlayApp::update`) both run.
struct TrayStatusRows {
    config: tray_icon::menu::MenuItem,
    model: tray_icon::menu::MenuItem,
    vision: tray_icon::menu::MenuItem,
    thinking: tray_icon::menu::MenuItem,
    requests: tray_icon::menu::MenuItem,
    last: tray_icon::menu::MenuItem,
    telemetry: tray_icon::menu::MenuItem,
}

thread_local! {
    static TRAY_STATUS: RefCell<Option<TrayStatusRows>> = const { RefCell::new(None) };
    /// Handle to the "Launch at Login" check item, for reverting its checkbox
    /// on a failed toggle. Main thread only (see [`TrayStatusRows`]).
    static TRAY_LAUNCH_AT_LOGIN_ITEM: RefCell<Option<tray_icon::menu::CheckMenuItem>> =
        const { RefCell::new(None) };
    /// Radio-style check items of the "Model" submenu (selectable profiles
    /// only, pool order) and their labels. Main thread only.
    static TRAY_MODEL_ITEMS: RefCell<Vec<(String, tray_icon::menu::CheckMenuItem)>> =
        const { RefCell::new(Vec::new()) };
}

/// Replace the Status submenu's Config row (reload outcome). Main thread only.
pub fn update_tray_config(text: &str) {
    TRAY_STATUS.with(|s| {
        if let Some(rows) = s.borrow().as_ref() {
            rows.config.set_text(text);
        }
    });
}

/// Reflect the active model profile: check exactly its item in the "Model"
/// submenu and name it in the Status row. Main thread only; no-op without a
/// tray or when `index` is out of range.
pub fn update_tray_model(index: usize) {
    TRAY_MODEL_ITEMS.with(|items| {
        let items = items.borrow();
        for (i, (_, item)) in items.iter().enumerate() {
            item.set_checked(i == index);
        }
        if let Some((label, _)) = items.get(index) {
            TRAY_STATUS.with(|s| {
                if let Some(rows) = s.borrow().as_ref() {
                    rows.model.set_text(format!("Model: {label}"));
                }
            });
        }
    });
}

/// Update the probe rows of the Status submenu once the startup probe lands.
/// Main thread only (see [`TrayStatusRows`]). No-op if the tray failed to build.
pub fn update_tray_probe(vision_supported: bool, thinking_label: &str) {
    TRAY_STATUS.with(|s| {
        if let Some(rows) = s.borrow().as_ref() {
            let vision = if vision_supported { "supported" } else { "not supported" };
            rows.vision.set_text(format!("Vision: {vision}"));
            // "ctrl" = whether thinking can be toggled via the API, not whether
            // the model thinks (e.g. Gemma always emits <thought>, uncontrollable).
            rows.thinking.set_text(format!("Thinking ctrl: {thinking_label}"));
        }
    });
}

/// Update the telemetry sink row with live shipped/dropped record counts.
/// Main thread only (see [`TrayStatusRows`]).
pub fn update_tray_telemetry(shipped: u64, dropped: u64) {
    TRAY_STATUS.with(|s| {
        if let Some(rows) = s.borrow().as_ref() {
            rows.telemetry
                .set_text(format!("Telemetry: {shipped} shipped, {dropped} dropped"));
        }
    });
}

/// Update the request-stats rows of the Status submenu after a request finishes.
/// `last` is a short label for the most recent outcome. Main thread only.
pub fn update_tray_requests(ok: u32, err: u32, last: &str) {
    TRAY_STATUS.with(|s| {
        if let Some(rows) = s.borrow().as_ref() {
            let total = ok + err;
            let rate = (err * 100).checked_div(total).unwrap_or(0);
            rows.requests
                .set_text(format!("Requests: {total} (✓{ok} ✗{err}, {rate}% err)"));
            rows.last.set_text(format!("Last: {last}"));
        }
    });
}

/// Decode the embedded tray icon PNG into an RGBA `tray_icon::Icon`.
fn load_tray_icon() -> tray_icon::Icon {
    let png_bytes = include_bytes!("../../assets/tray-icon-32.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes.as_slice()));
    let mut reader = decoder.read_info().expect("invalid tray icon PNG");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("unknown output buffer size")];
    let info = reader.next_frame(&mut buf).expect("failed to decode tray icon");
    buf.truncate(info.buffer_size());
    tray_icon::Icon::from_rgba(buf, info.width, info.height).expect("invalid RGBA icon data")
}

/// Initialize the system tray icon with a disabled version label and a Quit item.
/// On macOS this is the only way to quit the app (Accessory policy = no Dock icon).
/// The `TrayIcon` is intentionally leaked (process-lifetime resource).
pub fn init_tray(ctx: &eframe::egui::Context, models: &[TrayModel]) {
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
    use tray_icon::TrayIconBuilder;

    // "Model" submenu: one check item per selectable profile (radio-style, the
    // first is active at startup), disabled rows for profiles that failed to
    // build. Only shown when there is a choice to make.
    let model_items: Vec<CheckMenuItem> = models
        .iter()
        .filter(|m| m.unavailable.is_none())
        .enumerate()
        .map(|(i, m)| CheckMenuItem::new(&m.label, true, i == 0, None))
        .collect();
    let model_ids: Vec<tray_icon::menu::MenuId> =
        model_items.iter().map(|item| item.id().clone()).collect();
    let unavailable_items: Vec<MenuItem> = models
        .iter()
        .filter_map(|m| {
            m.unavailable
                .as_ref()
                .map(|why| MenuItem::new(format!("{} \u{2014} unavailable: {why}", m.label), false, None))
        })
        .collect();
    let model_menu = if models.len() > 1 {
        let mut refs: Vec<&dyn tray_icon::menu::IsMenuItem> = Vec::new();
        for item in &model_items {
            refs.push(item);
        }
        let sep = PredefinedMenuItem::separator();
        if !unavailable_items.is_empty() {
            refs.push(&sep);
            for item in &unavailable_items {
                refs.push(item);
            }
        }
        Some(Submenu::with_items("Model", true, &refs).expect("failed to create model submenu"))
    } else {
        None
    };
    TRAY_MODEL_ITEMS.with(|items| {
        *items.borrow_mut() = models
            .iter()
            .filter(|m| m.unavailable.is_none())
            .map(|m| m.label.clone())
            .zip(model_items.iter().cloned())
            .collect();
    });

    let about_item = MenuItem::new("About clip-llm", true, None);
    let about_id = about_item.id().clone();
    let config_item = MenuItem::new("Open Config", true, None);
    let config_id = config_item.id().clone();
    let reload_item = MenuItem::new("Reload Config", true, None);
    let reload_id = reload_item.id().clone();
    let settings_item = MenuItem::new("Settings\u{2026}", true, None);
    let settings_id = settings_item.id().clone();
    let launch_at_login_enabled = NativePlatform.launch_at_login_enabled();
    TRAY_LAUNCH_AT_LOGIN_STATE.store(launch_at_login_enabled, Ordering::SeqCst);
    let launch_at_login_item =
        CheckMenuItem::new("Launch at Login", true, launch_at_login_enabled, None);
    let launch_at_login_id = launch_at_login_item.id().clone();
    let quit_item = MenuItem::new("Quit", true, None);
    let quit_id = quit_item.id().clone();

    // Status submenu — read-only rows. Connection/config rows are known now;
    // the probe and request-stats rows start as placeholders and are filled in
    // from the main thread via update_tray_probe / update_tray_requests.
    let cfg = crate::config::get();
    let endpoint = cfg.api_endpoint().unwrap_or("(default)");
    let model = models
        .iter()
        .find(|m| m.unavailable.is_none())
        .map(|m| m.label.as_str())
        .or(cfg.api_model())
        .unwrap_or("(default)");
    let streaming = if cfg.api_streaming().unwrap_or(true) { "on" } else { "off" };
    let config_status = match crate::config::load_outcome() {
        crate::config::LoadOutcome::Loaded(p) => format!("Config: loaded ({})", p.display()),
        crate::config::LoadOutcome::NoFile => "Config: none (defaults)".to_string(),
        crate::config::LoadOutcome::Failed { reason, .. } => format!("Config: error — {reason}"),
    };
    let endpoint_row = MenuItem::new(format!("Endpoint: {endpoint}"), false, None);
    let model_row = MenuItem::new(format!("Model: {model}"), false, None);
    let streaming_row = MenuItem::new(format!("Streaming: {streaming}"), false, None);
    let config_row = MenuItem::new(config_status, false, None);
    let vision_row = MenuItem::new("Vision: probing…", false, None);
    let thinking_row = MenuItem::new("Thinking ctrl: probing…", false, None);
    let requests_row = MenuItem::new("Requests: none yet", false, None);
    let last_row = MenuItem::new("Last: —", false, None);
    let telemetry_status = match cfg.telemetry_url() {
        Some(url) => format!("Telemetry → {url} ({})", cfg.telemetry_level().unwrap_or("info")),
        None => "Telemetry: off".to_string(),
    };
    let telemetry_row = MenuItem::new(telemetry_status, false, None);
    let status_menu = Submenu::with_items(
        "Status",
        true,
        &[
            &endpoint_row,
            &model_row,
            &streaming_row,
            &config_row,
            &PredefinedMenuItem::separator(),
            &vision_row,
            &thinking_row,
            &PredefinedMenuItem::separator(),
            &requests_row,
            &last_row,
            &telemetry_row,
        ],
    )
    .expect("failed to create status submenu");
    // Keep handles to the dynamic rows for later main-thread updates.
    TRAY_STATUS.with(|s| {
        *s.borrow_mut() = Some(TrayStatusRows {
            config: config_row.clone(),
            model: model_row.clone(),
            vision: vision_row.clone(),
            thinking: thinking_row.clone(),
            requests: requests_row.clone(),
            last: last_row.clone(),
            telemetry: telemetry_row.clone(),
        });
    });
    TRAY_LAUNCH_AT_LOGIN_ITEM.with(|c| {
        *c.borrow_mut() = Some(launch_at_login_item.clone());
    });

    let menu = Menu::new();
    menu.append(&about_item).expect("failed to build tray menu");
    menu.append(&settings_item).expect("failed to build tray menu");
    menu.append(&config_item).expect("failed to build tray menu");
    menu.append(&reload_item).expect("failed to build tray menu");
    if let Some(model_menu) = &model_menu {
        menu.append(model_menu).expect("failed to build tray menu");
    }
    menu.append(&status_menu).expect("failed to build tray menu");
    menu.append(&PredefinedMenuItem::separator()).expect("failed to build tray menu");
    menu.append(&launch_at_login_item).expect("failed to build tray menu");
    menu.append(&PredefinedMenuItem::separator()).expect("failed to build tray menu");
    menu.append(&quit_item).expect("failed to build tray menu");

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("clip-llm")
        .with_icon(load_tray_icon())
        .build();

    match tray {
        Ok(tray) => {
            // Leak: the tray icon lives for the entire process lifetime.
            std::mem::forget(tray);

            // set_event_handler intercepts all menu events. Actions that must run
            // on the main thread are signalled via AtomicBool and executed by
            // poll_tray_events() inside update().
            let quit_id = quit_id.clone();
            let about_id = about_id.clone();
            let launch_at_login_id = launch_at_login_id.clone();
            let ctx = ctx.clone();
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                if event.id() == &quit_id {
                    TRAY_QUIT_REQUESTED.store(true, Ordering::SeqCst);
                    // Windows: a hidden window gets no WM_PAINT, so nudge it
                    // visible so update() runs and poll_tray_events() can act.
                    // Not needed on macOS.
                    #[cfg(target_os = "windows")]
                    windows::show_no_activate();
                    ctx.request_repaint();
                } else if event.id() == &about_id {
                    // macOS: NSAlert must run on the main thread — signal and let
                    // poll_tray_events() (inside update()) show it.
                    #[cfg(target_os = "macos")]
                    {
                        TRAY_ABOUT_REQUESTED.store(true, Ordering::SeqCst);
                        ctx.request_repaint();
                    }
                    // Windows: the message box runs on its own thread, so show it
                    // directly — no main-thread round-trip, and no window nudge
                    // (which would leave a stray transparent-but-visible overlay
                    // intercepting clicks while the state machine is Hidden).
                    #[cfg(target_os = "windows")]
                    windows::show_about();
                } else if event.id() == &config_id {
                    // Spawning an editor process needs no main-thread round-trip.
                    open_config_file();
                } else if event.id() == &settings_id {
                    TRAY_SETTINGS_REQUESTED.store(true, Ordering::SeqCst);
                    #[cfg(target_os = "windows")]
                    windows::show_no_activate();
                    ctx.request_repaint();
                } else if event.id() == &reload_id {
                    // The swap touches the worker and UI state, so it runs on
                    // the main thread inside poll_tray_events.
                    TRAY_RELOAD_REQUESTED.store(true, Ordering::SeqCst);
                    #[cfg(target_os = "windows")]
                    windows::show_no_activate();
                    ctx.request_repaint();
                } else if let Some(index) = model_ids.iter().position(|id| id == event.id()) {
                    // The native check item toggled itself; the main thread
                    // re-checks the whole radio group via update_tray_model
                    // once the state machine has applied the switch.
                    TRAY_MODEL_SELECTED.store(index, Ordering::SeqCst);
                    #[cfg(target_os = "windows")]
                    windows::show_no_activate();
                    ctx.request_repaint();
                } else if event.id() == &launch_at_login_id {
                    // The native checkbox has already auto-toggled its own visual
                    // by the time this event fires, so the target state is the
                    // opposite of the last confirmed state (see
                    // TRAY_LAUNCH_AT_LOGIN_STATE docs). The filesystem/registry
                    // write is cheap and thread-safe, so no main-thread
                    // round-trip is needed for the happy path.
                    let target = !TRAY_LAUNCH_AT_LOGIN_STATE.load(Ordering::SeqCst);
                    match NativePlatform.set_launch_at_login(target) {
                        Ok(()) => {
                            TRAY_LAUNCH_AT_LOGIN_STATE.store(target, Ordering::SeqCst);
                        }
                        Err(e) => {
                            tracing::warn!("failed to set launch-at-login to {target}: {e}");
                            // The checkbox now shows the wrong (attempted) state;
                            // ask the main thread to revert it to reality.
                            TRAY_LAUNCH_AT_LOGIN_REVERT.store(true, Ordering::SeqCst);
                            ctx.request_repaint();
                        }
                    }
                }
            }));

            tracing::info!("system tray icon created");
        }
        Err(e) => {
            tracing::warn!("failed to create tray icon: {e}");
        }
    }
}

/// Open the config file in a text editor, writing a commented starter template
/// first when no file exists yet (see `config::ensure_config_file`). Runs on
/// the tray menu handler thread.
pub fn open_config_file() {
    let Some(path) = crate::config::ensure_config_file() else {
        tracing::warn!("open config: no usable config path");
        return;
    };
    // `open -t` (default text editor) / notepad: deterministic even when no
    // app is associated with .toml files.
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg("-t").arg(&path).spawn();
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("notepad.exe").arg(&path).spawn();
    match spawned {
        Ok(mut child) => {
            tracing::info!("opening config file: {}", path.display());
            // Reap the launcher in the background so it never lingers as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!("failed to open config file: {e}"),
    }
}

/// Poll for tray menu actions inside `update()` (main thread): show the About
/// dialog (macOS only — NSAlert requires the main thread; Windows shows its
/// message box directly from the menu handler) and/or quit. Called every frame
/// from `OverlayApp::update`. Returns the action the caller must carry out
/// (model pick or config reload), if one was requested since the last poll.
pub fn poll_tray_events(ctx: &eframe::egui::Context) -> Option<TrayAction> {
    #[cfg(target_os = "macos")]
    if TRAY_ABOUT_REQUESTED.swap(false, Ordering::SeqCst) {
        macos::show_about();
    }
    let mut action = match TRAY_MODEL_SELECTED.swap(NO_MODEL_SELECTED, Ordering::SeqCst) {
        NO_MODEL_SELECTED => None,
        index => Some(TrayAction::SelectModel(index)),
    };
    if TRAY_RELOAD_REQUESTED.swap(false, Ordering::SeqCst) {
        action = action.or(Some(TrayAction::ReloadConfig));
    }
    if TRAY_SETTINGS_REQUESTED.swap(false, Ordering::SeqCst) {
        action = action.or(Some(TrayAction::OpenSettings));
    }
    if TRAY_LAUNCH_AT_LOGIN_REVERT.swap(false, Ordering::SeqCst) {
        // Re-query reality rather than assuming "not target" — a concurrent
        // successful toggle could have landed between the failed attempt and
        // this poll.
        let actual = NativePlatform.launch_at_login_enabled();
        TRAY_LAUNCH_AT_LOGIN_STATE.store(actual, Ordering::SeqCst);
        TRAY_LAUNCH_AT_LOGIN_ITEM.with(|c| {
            if let Some(item) = c.borrow().as_ref() {
                item.set_checked(actual);
            }
        });
    }
    if TRAY_QUIT_REQUESTED.swap(false, Ordering::SeqCst) {
        tracing::info!("quit requested from tray menu");
        ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
    }
    action
}

/// Show a native, modal alert with the given title and message, blocking until
/// the user dismisses it. Intended for fatal startup errors — before the tray,
/// window, or event loop exist — that would otherwise only reach stderr, which
/// is invisible when running as a windowless .app bundle.
pub fn show_startup_alert(title: &str, message: &str) {
    #[cfg(target_os = "macos")]
    macos::show_alert(title, message);
    #[cfg(target_os = "windows")]
    windows::show_alert_blocking(title, message);
}

/// Returns a platform-specific callback for pre-show hooks (coordinator / diagnostics threads).
///
/// On Windows, hidden windows (SW_HIDE) do not receive WM_PAINT, so eframe `update()`
/// never fires. This callback uses `SW_SHOWNA` to make the window visible without
/// stealing focus — keeping `SendInput(Ctrl+C)` targeting the correct foreground window.
///
/// On macOS, no-op — macOS uses `CGEvent` for copy simulation (focus-independent).
pub fn pre_show_callback() -> Box<dyn Fn() + Send> {
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::show_no_activate)
    }
    #[cfg(not(target_os = "windows"))]
    {
        Box::new(|| {})
    }
}
