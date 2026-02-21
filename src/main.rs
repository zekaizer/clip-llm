#![deny(unused_must_use)]

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tracing::{error, info, warn};

use clip_llm::api::client::LlmClient;
use clip_llm::api::response::strip_think_blocks;
use clip_llm::clipboard::ClipboardManager;
use clip_llm::hotkey::{HotkeyDetector, TapAction};
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

    info!("registered hotkey: Ctrl+Shift+C (single-tap: clipboard, double-tap: copy selection)");

    let mut detector = HotkeyDetector::new();
    let mut clipboard = ClipboardManager::new()?;
    let llm = LlmClient::new()?;
    let receiver = GlobalHotKeyEvent::receiver();

    info!("entering event loop");
    platform::run_event_loop(|| {
        while let Ok(event) = receiver.try_recv() {
            if event.state != HotKeyState::Pressed {
                continue;
            }

            match detector.on_press() {
                TapAction::Pending => {}
                TapAction::DoubleTap => {
                    info!("double-tap triggered, copying selection...");
                    if let Err(e) = handle_copy_pipeline(&mut clipboard, &plat, &llm) {
                        error!("pipeline error: {e}");
                    }
                }
            }
        }

        // Check if a single-tap has timed out.
        if detector.check_timeout() {
            info!("single-tap triggered, using clipboard content...");
            if let Err(e) = handle_clipboard_pipeline(&mut clipboard, &llm) {
                error!("pipeline error: {e}");
            }
        }
    });
}

/// Single-tap: read existing clipboard content and send to LLM.
fn handle_clipboard_pipeline(
    clipboard: &mut ClipboardManager,
    llm: &LlmClient,
) -> Result<(), AppError> {
    let input = clipboard.read_clipboard()?;
    process_llm(clipboard, llm, &input)
}

/// Double-tap: copy current selection, then send to LLM.
fn handle_copy_pipeline(
    clipboard: &mut ClipboardManager,
    platform: &dyn Platform,
    llm: &LlmClient,
) -> Result<(), AppError> {
    let input = clipboard.copy_and_read(platform)?;
    process_llm(clipboard, llm, &input)
}

/// Shared LLM processing: send input, strip think blocks, write response to clipboard.
fn process_llm(
    clipboard: &mut ClipboardManager,
    llm: &LlmClient,
    input: &str,
) -> Result<(), AppError> {
    info!("input: {} chars", input.len());

    let raw_response = llm.complete(input)?;
    let response = strip_think_blocks(&raw_response);

    if response.is_empty() {
        warn!("response empty after stripping think blocks");
        return Ok(());
    }

    clipboard.write_text(&response)?;
    info!("pipeline complete, response: {} chars", response.len());
    Ok(())
}
