# clip-llm — System-wide LLM Clipboard Assistant

## Project Overview
A cross-platform, single-binary desktop daemon that captures clipboard/selection text via global hotkey, sends it to an internal vLLM server (OpenAI-compatible API), and writes the response back to the clipboard.

## Target Environment
- Development & primary platform: macOS
- Secondary platform: Win11
- Network: intranet only, no external access
- Backend: internal vLLM server (OpenAI-compatible `/v1/chat/completions`)
- Distribution: single binary, no installer, no runtime dependencies

## Tech Stack
- Language: Rust (2021 edition, stable)
- Key crates:
  - `global-hotkey` — system-wide hotkey registration
  - `arboard` — clipboard read/write
  - `reqwest` — HTTP client (blocking in Phase 1, async in Phase 2), feature `rustls-tls`
  - `serde`, `serde_json` — JSON serialization
  - `thiserror` — typed error definitions
  - `regex` — think-block stripping
  - `tracing` — structured logging
  - `dirs` — platform-specific config/data directories
  - `toml` — config file parsing (Phase 4+)
  - `tray-icon` — system tray (Phase 3+)
  - `notify-rust` — toast notifications (Phase 3+)
  - `tokio` — async runtime (Phase 2+)

## Architecture

### Platform Abstraction
All platform-specific code is isolated behind a common trait from Phase 1. Both macOS and Windows implementations are maintained in parallel.

```
src/
  lib.rs            // Library crate root, re-exports all modules
  main.rs           // Binary crate: event loop bootstrap, wiring only
  platform/
    mod.rs          // Platform trait + re-export via cfg
    macos.rs        // macOS: CGEvent, Accessibility
    windows.rs      // Windows: SendInput, Win32
```

`main.rs` (binary crate) is a thin entry point — event loop setup, signal handling. All business logic lives in `lib.rs` and submodules (library crate), keeping it testable independently.

Platform trait covers:
- `simulate_copy()` — key simulation for copy (CGEvent / SendInput)
- `check_accessibility()` — macOS: prompt for Accessibility permission, Windows: no-op

Platform-independent values handled outside the trait:
- Hotkey: `Ctrl+Shift+C` single-tap (clipboard) / double-tap (copy selection) on both platforms
- Config directory: `dirs::config_dir()` (cross-platform)

### Event Loop
The main thread owns the platform event loop (NSApplication run loop on macOS, message loop on Windows). `global-hotkey` and `tray-icon` (Phase 3+) require this. When `tokio` is introduced (Phase 2), the async runtime runs on a **separate thread**, communicating with the main thread via channels.

### State Management (Phase 2+)
Channel-based message passing between the event loop (main thread) and async workers:
- Hotkey event → command channel → worker
- Worker result → response channel → main thread → clipboard write
- In-flight request cancellation via `tokio::sync::oneshot` or `CancellationToken`
- Phase 1 uses synchronous flow on the main thread (no channels needed)

### Error Handling
- `thiserror` for typed errors (HTTP, parse, clipboard, platform)
- Categorized for retry logic (Phase 3): transient (5xx, timeout) vs permanent (4xx, parse)
- Never panic on user-facing errors

---

## Phase 1 — Basic Pipeline

### Scope
Hardcoded single prompt template. macOS + Windows. Console log for status. Blocking HTTP. Platform abstraction layer.

### Features
1. **Platform abstraction layer**
   - Implement platform trait (see Architecture) for macOS and Windows
   - `cfg(target_os)` compile-time selection

2. **Global hotkey registration**
   - Register `Ctrl+Shift+C` via `global-hotkey` (same on both platforms)
   - Two activation modes via tap detection (500ms timeout window):
     - **Single-tap**: use existing clipboard content as LLM input
     - **Double-tap**: copy current selection first, then use as LLM input
   - macOS: requires Accessibility permission (detect and prompt)
   - Windows: no special permission required

3. **Clipboard read**
   - **Single-tap mode**: read current clipboard text via `arboard` directly
   - **Double-tap mode**: simulate copy via platform trait (`CGEvent` on macOS, `SendInput` on Windows), then poll clipboard for change up to 2s

4. **vLLM API call (blocking)**
   - POST to hardcoded endpoint (`http://<host>:8000/v1/chat/completions`)
   - Hardcoded model name, system prompt, temperature, max_tokens
   - Request body follows OpenAI chat completions schema

5. **Response parsing**
   - Extract `choices[0].message.content` from JSON response
   - Strip `<think>...</think>` blocks from response (regex: `(?s)<think>.*?</think>`)
   - Trim leading/trailing whitespace after stripping

6. **Clipboard write**
   - Write cleaned response to clipboard

7. **Console logging**
   - Log each step: hotkey triggered, clipboard read, API request sent, response received, clipboard written
   - Log errors with context (HTTP status, parse failure, timeout)

### Non-functional
- Single binary, `cargo build --release`
- No config file, all values hardcoded as constants
- Graceful error handling, never panic on user-facing errors

---

## Phase 2 — Async API + SSE Streaming

### Scope
Replace blocking HTTP with async. Add SSE streaming support.

### Features
1. **Async runtime**
   - Migrate to `tokio` runtime
   - `reqwest` async client with connection pooling

2. **SSE streaming**
   - Request with `"stream": true`
   - Parse `data: {...}` lines, extract `choices[0].delta.content`
   - Accumulate chunks, write final result to clipboard on `[DONE]`
   - Strip `<think>...</think>` blocks from accumulated response

3. **Timeout handling**
   - Configurable request timeout (default: 30s)
   - Cancel in-flight request on next hotkey press

4. **Process mode switching**
   - `ProcessMode` enum: `Translate` (default), `Correct`, `Summarize`
   - Extensible: add new modes by extending the enum + `ALL` array
   - Each mode has its own system prompt via `ProcessMode::system_prompt()`
   - Image-only clipboard content auto-selects `Summarize` mode (vision API)

5. **Mode tab bar in overlay**
   - Top of overlay shows tab bar with all modes from `ProcessMode::ALL`
   - Selected tab: white text + highlighted background; unselected: gray + transparent
   - Dynamically renders from `ALL` — no UI code change needed to add modes

6. **Mode switching behavior**
   - During Processing: cancel current request, re-send with new mode
   - On Result/Error: re-process original input text with new mode
   - Mode persists between invocations (next hotkey trigger uses last selected mode)
   - Original input text retained for re-processing on mode switch

---

## Phase 3 — Status Feedback + System Tray

### Scope
User-visible feedback and background daemon UX.

### Features
1. **Toast notifications**
   - "Processing..." on API call start
   - "Done — copied to clipboard" on success
   - "Error: ..." on failure with brief description

2. **System tray icon**
   - Idle / processing state indicator
   - Tray menu:
     - Current template name (display only)
     - Reload config (Phase 4+)
     - Quit

3. **Error retry**
   - Auto-retry once on transient HTTP errors (5xx, timeout)
   - Show error notification if retry also fails

---

## Phase 4 — Config File + Multiple Templates

### Scope
Externalize all hardcoded values. Support multiple prompt templates.

### Features
1. **TOML config file**
   - Path: `~/.config/clip-llm/config.toml` (macOS/Linux), `%APPDATA%\clip-llm\config.toml` (Windows)
   - Auto-create default config on first run if missing

2. **Config schema**
   ```toml
   [api]
   endpoint = "http://host:8000/v1/chat/completions"
   model = "model-name"
   api_key = ""  # optional
   timeout_secs = 30

   [defaults]
   temperature = 0.3
   max_tokens = 1024
   strip_think = true

   [[templates]]
   name = "Jira"
   system_prompt = "You are a Jira ticket formatting assistant."
   user_prompt_prefix = "Convert to Jira ticket format:\n"
   temperature = 0.3
   max_tokens = 1024
   strip_think = true
   order = 1

   [[templates]]
   name = "Code Review"
   system_prompt = "You are a code review assistant."
   user_prompt_prefix = "Review the following code:\n"
   temperature = 0.2
   max_tokens = 2048
   strip_think = false
   order = 2
   ```

3. **Template-level overrides**
   - `temperature`, `max_tokens`, `strip_think` per template
   - Falls back to `[defaults]` if omitted

4. **Cycle order**
   - `order` field determines hotkey cycle sequence (ascending)

---

## Phase 5 — Template Cycle Selection UI

### Scope
Alt+Tab style template switcher.

### Features
1. **Modifier-hold + repeat key cycling**
   - Hold `Ctrl+Shift`, press `C` repeatedly to cycle through templates
   - Cycle follows `order` field from config
   - Wraps around at end of list

2. **Overlay display**
   - Small floating overlay showing current template name
   - Appears on first keypress, updates on each subsequent press
   - Positioned near cursor or screen center

3. **Selection confirm**
   - Release modifier keys → confirm selection → start API call
   - Timeout auto-confirm after configurable interval (default: 3s of no input)

4. **Single template fallback**
   - If only one template exists, skip cycling, execute immediately

---

## Phase 6 — Windows Build & Distribution

### Scope
Windows CI, testing, and distribution. Platform implementations already exist from Phase 1.

### Features
1. **Windows build verification**
   - CI build for `x86_64-pc-windows-gnu` target
   - End-to-end testing on Windows 11

2. **Single binary distribution**
   - Portable `.exe`, no installer
   - Cross-compile from macOS: `cargo build --release --target x86_64-pc-windows-gnu`
   - Native build on Windows: `cargo build --release`

---

## Phase 7 — Extended Features

### Features
1. **Response history**
   - Local storage (SQLite or JSON file)
   - Store: timestamp, template used, input text (truncated), response, think block (if any)

2. **Config hot-reload**
   - Watch config file for changes (`notify` crate)
   - Reload templates and settings without restart

3. **Per-template direct hotkeys**
   - Optional `hotkey` field per template for direct invocation bypassing cycle UI

---

## Coding Conventions
- All code, comments, variable names, function names, struct names, macros, and string literals in English
- Error messages in English
- Documentation comments (`///`) in English
- `clippy` clean, no warnings
- `#[deny(unused_must_use)]`
