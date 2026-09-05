#![deny(unused_must_use)]

use std::sync::{mpsc, Arc};

use eframe::egui;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
use tracing::{debug, error, info, warn};

use clip_llm::api::client::LlmClient;
use clip_llm::clipboard::ClipboardManager;
use clip_llm::hotkey::TapEvent;
use clip_llm::ui::OverlayApp;
use clip_llm::worker::{spawn_worker, WorkerCommand, WorkerResponse};
use clip_llm::platform::TrayModel;
use clip_llm::HotkeyError;

fn main() {
    // Config must load before logging init: the `[logging]` section decides
    // whether the VictoriaLogs sink is attached. `init()` is idempotent
    // (OnceLock), so the later call inside `run()` is a no-op.
    clip_llm::config::init();
    clip_llm::telemetry::init();
    debug!("debug logging enabled");
    info!("clip-llm v{} by {}", env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_AUTHORS"));

    if let Err(e) = run() {
        error!("fatal: {e}");
        std::process::exit(1);
    }
}

/// Configure fonts with embedded D2Coding (zstd-compressed) for broad Unicode + Korean coverage.
fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    let compressed = include_bytes!(concat!(env!("OUT_DIR"), "/D2Coding.ttf.zst"));
    let font_bytes = zstd::decode_all(&compressed[..]).expect("failed to decompress font");
    let font_data = egui::FontData::from_owned(font_bytes);
    fonts
        .font_data
        .insert("d2coding".to_owned(), font_data.into());

    // Use D2Coding as primary font for both proportional and monospace.
    fonts
        .families
        .get_mut(&egui::FontFamily::Proportional)
        .unwrap()
        .insert(0, "d2coding".to_owned());
    fonts
        .families
        .get_mut(&egui::FontFamily::Monospace)
        .unwrap()
        .insert(0, "d2coding".to_owned());

    ctx.set_fonts(fonts);
}

/// Select the best wgpu adapter: prefer hardware GPU, fall back to software (WARP on Windows).
fn select_wgpu_adapter(
    adapters: &[wgpu::Adapter],
    _surface: Option<&wgpu::Surface<'_>>,
) -> Result<wgpu::Adapter, String> {
    for (i, a) in adapters.iter().enumerate() {
        let info = a.get_info();
        info!(
            "wgpu adapter[{i}]: {} ({:?}, {:?})",
            info.name, info.device_type, info.backend
        );
    }

    let hw = adapters
        .iter()
        .find(|a| a.get_info().device_type == wgpu::DeviceType::DiscreteGpu)
        .or_else(|| {
            adapters
                .iter()
                .find(|a| a.get_info().device_type == wgpu::DeviceType::IntegratedGpu)
        })
        .or_else(|| {
            adapters
                .iter()
                .find(|a| a.get_info().device_type != wgpu::DeviceType::Cpu)
        });

    let selected = if let Some(a) = hw {
        a.clone()
    } else {
        let sw = adapters
            .first()
            .cloned()
            .ok_or_else(|| "no wgpu adapter found".to_string())?;
        let info = sw.get_info();
        warn!(
            "no hardware GPU — falling back to software adapter: {} ({:?})",
            info.name, info.backend
        );
        sw
    };

    let info = selected.get_info();
    info!(
        "wgpu selected: {} ({:?}, {:?})",
        info.name, info.device_type, info.backend
    );
    Ok(selected)
}

/// Show a user-visible alert when the global hotkey cannot be registered — the
/// most common real-world causes are a second clip-llm instance already
/// running, or another app having reserved the Ctrl+Shift+C combination.
fn show_hotkey_failure_alert(e: &HotkeyError) {
    let message = format!(
        "clip-llm could not register the Ctrl+Shift+C hotkey ({e}).\n\n\
         This usually means either:\n\
         - clip-llm is already running, or\n\
         - another app has reserved this key combination."
    );
    clip_llm::platform::show_startup_alert("clip-llm: hotkey registration failed", &message);
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Load prompt overrides from config.toml (if any) before any worker or UI
    // thread reads them. Falls back to built-in defaults on any error.
    clip_llm::config::init();

    // A failed config load is surfaced via the overlay once the UI is up —
    // the stderr fallback is invisible in the .app bundle distribution.
    let startup_notice = match clip_llm::config::load_outcome() {
        clip_llm::config::LoadOutcome::Failed { path, reason } => Some(format!(
            "Config file ignored ({reason}): {} — using built-in defaults.",
            path.display()
        )),
        _ => None,
    };

    // Check platform permissions before anything else. On macOS this also shows
    // the system permission dialog; if still denied, print an actionable path
    // (the app has no window/Dock icon on this code path, so stderr is the only
    // feedback channel).
    {
        use clip_llm::platform::{NativePlatform, Platform};
        if let Err(e) = NativePlatform.check_accessibility() {
            #[cfg(target_os = "macos")]
            eprintln!(
                "\nclip-llm needs Accessibility permission to simulate Cmd+C / Cmd+V.\n\
                 Grant it in: System Settings > Privacy & Security > Accessibility\n\
                 (enable clip-llm), then relaunch.\n"
            );
            return Err(e.into());
        }
    }

    // GlobalHotKeyManager must be created on the main thread and kept alive.
    // A failure here is usually caused by another clip-llm instance already
    // running, or another app having reserved Ctrl+Shift+C — but with no
    // window/Dock icon on this code path, stderr is invisible in the .app
    // bundle, so also show a native alert (mirrors the accessibility and
    // missing-config startup paths above/below).
    let manager = match GlobalHotKeyManager::new() {
        Ok(m) => m,
        Err(e) => {
            let err = HotkeyError::InitFailed(e.to_string());
            show_hotkey_failure_alert(&err);
            return Err(err.into());
        }
    };
    let hotkey = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyC);
    if let Err(e) = manager.register(hotkey) {
        let err = HotkeyError::RegisterFailed(e.to_string());
        show_hotkey_failure_alert(&err);
        return Err(err.into());
    }
    info!("registered hotkey: Ctrl+Shift+C (single-tap: clipboard, double-tap: copy selection)");

    // Set up channels and spawn the async worker thread.
    // Command channel uses tokio::sync::mpsc so worker can .recv().await
    // without blocking the single-threaded tokio runtime.
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<WorkerCommand>();
    let (resp_tx, resp_rx) = mpsc::channel::<WorkerResponse>();
    // The required api.endpoint/model/api_key have no defaults. When unset, the
    // app cannot run and — since it exits before the tray exists — the user can't
    // reach Open Config. So write the starter template here, point at it, and
    // exit so the next launch finds a file to fill in.
    // Model profiles: [api] first, then each [[models]] entry. The first
    // (default) profile must build or the app cannot run; a later one that
    // fails is listed in the tray as unavailable with the reason instead of
    // taking the whole app down.
    let specs = clip_llm::config::get()
        .model_specs()
        .map_err(clip_llm::ApiError::InvalidConfig)?;
    let mut clients = Vec::with_capacity(specs.len());
    let mut tray_models = Vec::with_capacity(specs.len());
    let mut unavailable = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        match LlmClient::for_spec(spec) {
            Ok(llm) => {
                clients.push(llm);
                tray_models.push(TrayModel { label: spec.name.clone(), unavailable: None });
            }
            Err(e @ clip_llm::ApiError::MissingConfig(_)) if index == 0 => {
                match clip_llm::config::ensure_config_file() {
                    Some(path) => error!(
                        "{e}. Wrote a starter config to {} — set the required keys there and relaunch.",
                        path.display()
                    ),
                    None => error!("{e}"),
                }
                return Err(e.into());
            }
            Err(e) if index == 0 => return Err(e.into()),
            Err(e) => {
                warn!("model profile {:?} unavailable: {e}", spec.name);
                unavailable.push(TrayModel {
                    label: spec.name.clone(),
                    unavailable: Some(e.to_string()),
                });
            }
        }
    }
    tray_models.extend(unavailable);
    let model_count = clients.len();
    let clipboard = ClipboardManager::new()?;

    info!("starting eframe overlay");

    eframe::run_native(
        "clip-llm",
        build_native_options(),
        Box::new(move |cc| {
            configure_fonts(&cc.egui_ctx);
            // Transparent background for the overlay viewport (one-time setup).
            cc.egui_ctx.set_visuals(egui::Visuals {
                window_fill: egui::Color32::TRANSPARENT,
                panel_fill: egui::Color32::TRANSPARENT,
                window_stroke: egui::Stroke::NONE,
                window_shadow: egui::Shadow::NONE,
                window_corner_radius: egui::CornerRadius::same(12),
                ..egui::Visuals::dark()
            });

            // Forward hotkey events to coordinator thread (no request_repaint here —
            // coordinator handles wake-up after detecting tap action).
            let (hotkey_tx, hotkey_rx) = mpsc::channel();
            GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
                let _ = hotkey_tx.send(event);
            }));

            // System tray icon (Windows: replaces taskbar icon).
            clip_llm::platform::init_tray(&cc.egui_ctx, &tray_models);

            // Listen-only OS keyboard watcher for the Ctrl+Shift release (commit)
            // signal that global-hotkey cannot observe. Accessibility was already
            // verified above, so the macOS tap installs with the same permission.
            let modifier_state = clip_llm::platform::spawn_modifier_watcher();

            // Coordinator thread: event-driven hotkey detection (off-UI).
            // Cloned: the same live state is also attached to each selection
            // capture's ClipboardManager (via OverlayApp) so copy_and_read can
            // wait for an actual modifier release instead of a flat delay.
            let tap_rx = spawn_coordinator_thread(
                hotkey_rx,
                cc.egui_ctx.clone(),
                modifier_state.clone(),
            );

            // Diagnostics: spawn scenario runner thread (off-UI, like coordinator).
            #[cfg(feature = "diagnostics")]
            let (diag_action_rx, diag_state_tx) = {
                let (action_tx, action_rx) = mpsc::channel();
                let (state_tx, state_rx) = mpsc::channel();
                let ctx_for_diag = cc.egui_ctx.clone();
                let pre_show_diag = clip_llm::platform::pre_show_callback();
                std::thread::spawn(move || {
                    clip_llm::diagnostics::run_scenario_thread(
                        state_rx, action_tx, ctx_for_diag, pre_show_diag,
                    );
                });
                (action_rx, state_tx)
            };

            // Worker thread: async LLM calls. Spawned here (not before
            // run_native) so it gets the egui Context and can wake the UI loop
            // when the one-shot startup probe completes.
            let _worker = spawn_worker(cmd_rx, resp_tx, clients, cc.egui_ctx.clone());

            #[cfg(feature = "diagnostics")]
            let app = OverlayApp::new(
                cmd_tx, resp_rx, clipboard, tap_rx, modifier_state,
                diag_action_rx, diag_state_tx,
            );
            #[cfg(not(feature = "diagnostics"))]
            let app = OverlayApp::new(cmd_tx, resp_rx, clipboard, tap_rx, modifier_state);
            let app = app
                .with_startup_notice(startup_notice)
                .with_model_count(model_count);

            Ok(Box::new(app))
        }),
    )?;

    Ok(())
}

/// Build eframe native options: overlay viewport geometry, wgpu adapter selection,
/// and macOS Accessory activation policy.
fn build_native_options() -> eframe::NativeOptions {
    let viewport = egui::ViewportBuilder::default()
        .with_title("clip-llm")
        .with_inner_size([400.0, 120.0])
        .with_visible(false)
        .with_decorations(false)
        .with_resizable(false)
        .with_always_on_top()
        .with_transparent(true)
        .with_taskbar(false);

    eframe::NativeOptions {
        viewport,
        wgpu_options: egui_wgpu::WgpuConfiguration {
            wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(egui_wgpu::WgpuSetupCreateNew {
                native_adapter_selector: Some(Arc::new(select_wgpu_adapter)),
                ..Default::default()
            }),
            ..Default::default()
        },
        // Accessory policy: no Dock icon, no Cmd+Tab, no "home Space".
        // Prevents macOS from switching Spaces when the app shows a window.
        #[cfg(target_os = "macos")]
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
            builder.with_activation_policy(ActivationPolicy::Accessory);
        })),
        ..Default::default()
    }
}

/// Spawn the coordinator thread and return the tap event receiver.
///
/// The coordinator blocks on hotkey events, runs [`HotkeyDetector`] tap detection,
/// and sends [`TapEvent`]s to the UI via the returned channel.
fn spawn_coordinator_thread(
    hotkey_rx: mpsc::Receiver<GlobalHotKeyEvent>,
    ctx: egui::Context,
    modifier_state: clip_llm::platform::ModifierState,
) -> mpsc::Receiver<TapEvent> {
    let pre_show = clip_llm::platform::pre_show_callback();
    let mouse_pos_fn: Box<dyn Fn() -> Option<(f64, f64)> + Send> = {
        use clip_llm::platform::{NativePlatform, Platform};
        Box::new(|| NativePlatform.mouse_position())
    };
    // Resolve the double-tap window from config; a zero (which would make
    // double-tap impossible) or missing value falls back to the built-in default.
    let double_tap_timeout = clip_llm::config::get()
        .hotkey_double_tap_timeout_ms()
        .filter(|&ms| ms > 0)
        .map(std::time::Duration::from_millis)
        .unwrap_or(clip_llm::hotkey::DEFAULT_DOUBLE_TAP_TIMEOUT);
    info!("double-tap window: {}ms", double_tap_timeout.as_millis());
    let (tap_tx, tap_rx) = mpsc::channel::<TapEvent>();
    std::thread::spawn(move || {
        clip_llm::coordinator::run(
            hotkey_rx,
            tap_tx,
            ctx,
            pre_show,
            mouse_pos_fn,
            double_tap_timeout,
            modifier_state,
        );
    });
    tap_rx
}
