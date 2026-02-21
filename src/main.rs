#![deny(unused_must_use)]

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tracing::{error, info, warn};

use clip_llm::api::client::LlmClient;
use clip_llm::api::response::strip_think_blocks;
use clip_llm::clipboard::ClipboardManager;
use clip_llm::hotkey::DoubleTapDetector;
use clip_llm::platform::{self, NativePlatform, Platform};
use clip_llm::{AppError, HotkeyError};

fn main() {
    tracing_subscriber::fmt::init();
    info!("clip-llm starting");

    if let Err(e) = run() {
        error!("fatal: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), AppError> {
    let plat = NativePlatform;
    plat.check_accessibility()?;
    platform::init();

    let manager = GlobalHotKeyManager::new()
        .map_err(|e| HotkeyError::InitFailed(e.to_string()))?;

    let hotkey = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyC);

    manager
        .register(hotkey)
        .map_err(|e| HotkeyError::RegisterFailed(e.to_string()))?;

    info!("registered hotkey: Ctrl+Shift+C (double-tap to activate)");

    let mut detector = DoubleTapDetector::new();
    let mut clipboard = ClipboardManager::new()?;
    let llm = LlmClient::new()?;
    let receiver = GlobalHotKeyEvent::receiver();

    info!("entering event loop");
    platform::run_event_loop(|| {
        while let Ok(event) = receiver.try_recv() {
            if event.state != HotKeyState::Pressed {
                continue;
            }

            if !detector.on_press() {
                continue;
            }

            info!("double-tap triggered, processing...");
            if let Err(e) = handle_pipeline(&mut clipboard, &plat, &llm) {
                error!("pipeline error: {e}");
            }
        }
    });
}

fn handle_pipeline(
    clipboard: &mut ClipboardManager,
    platform: &dyn Platform,
    llm: &LlmClient,
) -> Result<(), AppError> {
    let input = clipboard.read_text(platform)?;
    info!("input: {} chars", input.len());

    let raw_response = llm.complete(&input)?;
    let response = strip_think_blocks(&raw_response);

    if response.is_empty() {
        warn!("response empty after stripping think blocks");
        return Ok(());
    }

    clipboard.write_text(&response)?;
    info!("pipeline complete, response: {} chars", response.len());
    Ok(())
}
