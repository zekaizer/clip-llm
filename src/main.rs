#![deny(unused_must_use)]

use std::sync::mpsc;

use eframe::egui;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::GlobalHotKeyManager;
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

use clip_llm::api::client::LlmClient;
use clip_llm::clipboard::ClipboardManager;
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
        .with_transparent(true);

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "clip-llm",
        native_options,
        Box::new(move |cc| {
            configure_fonts(&cc.egui_ctx);
            Ok(Box::new(OverlayApp::new(cmd_tx, resp_rx, clipboard)))
        }),
    )?;

    Ok(())
}
