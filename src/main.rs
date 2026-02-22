#![deny(unused_must_use)]

use std::sync::{mpsc, Arc};

use eframe::egui;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use clip_llm::api::client::LlmClient;
use clip_llm::clipboard::ClipboardManager;
use clip_llm::hotkey::TapEvent;
use clip_llm::ui::OverlayApp;
use clip_llm::worker::{spawn_worker, WorkerCommand, WorkerResponse};
use clip_llm::HotkeyError;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("clip_llm=info")),
        )
        .init();
    debug!("debug logging enabled");
    info!("clip-llm v{} starting", env!("CARGO_PKG_VERSION"));

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

fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Check platform permissions before anything else.
    {
        use clip_llm::platform::{NativePlatform, Platform};
        let plat = NativePlatform;
        plat.check_accessibility()?;
    }

    // GlobalHotKeyManager must be created on the main thread and kept alive.
    let manager = GlobalHotKeyManager::new()
        .map_err(|e| HotkeyError::InitFailed(e.to_string()))?;

    let hotkey = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyC);

    manager
        .register(hotkey)
        .map_err(|e| HotkeyError::RegisterFailed(e.to_string()))?;

    info!("registered hotkey: Ctrl+Shift+C (single-tap: clipboard, double-tap: copy selection)");

    // Set up channels between main thread and worker.
    // Command channel uses tokio::sync::mpsc so worker can .recv().await
    // without blocking the single-threaded tokio runtime.
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<WorkerCommand>();
    let (resp_tx, resp_rx) = mpsc::channel::<WorkerResponse>();

    let llm = LlmClient::new()?;
    let clipboard = ClipboardManager::new()?;

    // Spawn the async worker thread.
    let _worker = spawn_worker(cmd_rx, resp_tx, llm);

    info!("starting eframe overlay");

    let viewport = egui::ViewportBuilder::default()
        .with_title("clip-llm")
        .with_inner_size([400.0, 120.0])
        .with_visible(false)
        .with_decorations(false)
        .with_resizable(false)
        .with_always_on_top()
        .with_transparent(true)
        .with_taskbar(false);

    let native_options = eframe::NativeOptions {
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
    };

    eframe::run_native(
        "clip-llm",
        native_options,
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
            clip_llm::platform::init_tray(&cc.egui_ctx);

            // Platform-specific pre-show callback for coordinator thread.
            // On Windows, shows window natively before sending TapAction so that
            // WM_PAINT is delivered and eframe update() fires.
            let pre_show = clip_llm::platform::pre_show_callback();

            // Coordinator thread: event-driven hotkey detection (off-UI).
            let (tap_tx, tap_rx) = mpsc::channel::<TapEvent>();
            let ctx_for_coord = cc.egui_ctx.clone();
            let mouse_pos_fn: Box<dyn Fn() -> Option<(f64, f64)> + Send> = {
                use clip_llm::platform::{NativePlatform, Platform};
                Box::new(|| NativePlatform.mouse_position())
            };
            std::thread::spawn(move || {
                clip_llm::coordinator::run(
                    hotkey_rx,
                    tap_tx,
                    ctx_for_coord,
                    pre_show,
                    mouse_pos_fn,
                );
            });

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

            #[cfg(feature = "diagnostics")]
            let app = OverlayApp::new(
                cmd_tx, resp_rx, clipboard, tap_rx,
                diag_action_rx, diag_state_tx,
            );
            #[cfg(not(feature = "diagnostics"))]
            let app = OverlayApp::new(cmd_tx, resp_rx, clipboard, tap_rx);

            Ok(Box::new(app))
        }),
    )?;

    Ok(())
}
