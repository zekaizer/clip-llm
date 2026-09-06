# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Summary

**clip-llm** is a cross-platform single-binary desktop daemon (Rust) that captures clipboard/selection text via global hotkey, sends it to an internal vLLM server (OpenAI-compatible API), and writes the response back to the clipboard.

Primary platform: macOS. Secondary: Windows 11. Intranet only.

## Build & Run

```bash
cargo build --release          # release build
cargo run                      # dev run
cargo clippy -- -D warnings    # lint (must pass clean)
cargo test                     # run all tests
cargo test <test_name>         # run single test
```

Cross-compile to Windows (from macOS): `cargo build --release --target x86_64-pc-windows-gnu`

### Diagnostics

```bash
DIAG_MOCK=1 cargo run --features diagnostics   # mock LLM, auto-run all scenarios
cargo run --features diagnostics                # real LLM, auto-run all scenarios
```

Runs 7 test scenarios automatically (short text, long scroll, mode switch, error, Korean, correct mode, text wrapping), captures a screenshot + JSON sidecar per state transition, then exits. Output: `target/diagnostics/`. See `src/diagnostics.rs` for details.

## Workflow

- Before starting work, check open GitHub issues with `gh issue list` to understand current priorities and context.
- Create a feature branch per feature: `phase<N>-<feature>` (e.g. `phase1-api-client`, `phase2-sse-streaming`).
- Create a fix branch per bug fix: `fix-<description>` (e.g. `fix-macos-hotkey-events`).
- Merge with `--no-ff` to create a merge commit. Exception: single-commit branches can be fast-forward merged.
- Commit per feature/logical unit during implementation — do not batch unrelated changes into a single commit.
- Commit messages in English, following [Conventional Commits](https://www.conventionalcommits.org/) format (e.g. `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`).

## Coding Conventions

- All code, comments, variable/function/struct names, macros, string literals, error messages, doc comments (`///`) in **English**
- `clippy` clean, no warnings
- `#[deny(unused_must_use)]`
- Never panic on user-facing errors — graceful error handling throughout
- Single binary, no runtime dependencies

## Architecture

The project follows a 7-phase incremental plan defined in [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md). Progress is tracked in [README.md](README.md#roadmap).

### Structure

- `src/main.rs` — binary crate: event loop bootstrap, wiring only
- `src/lib.rs` — library crate: type definitions (`ProcessMode`, errors), re-exports
- `src/coordinator.rs` — hotkey event → tap detection → `TapEvent` dispatch (dedicated thread)
- `src/hotkey.rs` — `HotkeyDetector` state machine, `TapAction`, `TapEvent`
- `src/clipboard.rs` — `ClipboardManager` (read/write, copy simulation + poll)
- `src/files.rs` — file-list clipboard ingestion (`ingest_files`: UTF-8 text + image files → `ClipboardContent`)
- `src/images/` — everything an image goes through before a request: `ImageAttachment` (bytes + MIME + source/sent size + origin), `cap_for_request` (4 images / 8 MiB); `decode.rs` (PNG/JPEG/GIF/WebP/BMP → RGBA, pixel budget from the header), `encode.rs` (margin trim → 1568px → PNG, JPEG fallback over 1.5 MB), `filter.rs` (meaningfulness: markup prefilter + pixel checks + `select_top`), `html.rs` (`<img>` refs from `public.html` / CF_HTML), `fetch.rs` (`data:` / `file:` / `http(s)` bytes, bounded), `markup.rs` (the pipeline over an injected fetcher). Only markup images are filtered; clipboard/file images are deliberate
- `src/lang.rs` — input-language cues for Translate: `prose_is_korean` (direction, ADR-0002) and `is_lookup` (dictionary entry); fixture `scripts/lang_direction_vectors.json` shared with the harness
- `src/settings.rs` — settings panel model (`SettingsForm` → `SettingsPatch` → `toml_edit` in-place write; ADR-0001)
- `src/worker.rs` — async LLM request worker thread
- `src/api/client.rs` — `LlmClient` (`ApiFlavor`: OpenAI chat vs xAI Responses), `SseParser`, vision probe
- `src/api/response.rs` — `strip_think_blocks`, `ThinkBlockFilter` (streaming)
- `src/api/auth/` — `TokenProvider` (static key vs OAuth); `oauth.rs` generic refresh grant, `grok.rs` Grok CLI credential-store adapter (`~/.grok/auth.json` read/refresh/write-back)
- `src/ui/mod.rs` — `OverlayApp` (eframe::App adapter, effect execution, window management, panel size + grip persistence)
- `src/ui/state_machine.rs` — pure state machine (`OverlayState`, `UiEvent`, `UiEffect`), unit-testable
- `src/ui/overlay.rs` — views: `render()` composes the panel slots per state, `render_tab_bar()`, settings panel
- `src/ui/panel.rs` — fixed-size frame + `Slots` (header / status / body / footer) + resize grip
- `src/ui/theme.rs` — design tokens (type scale, palette, spacing, geometry) — the only place literals live
- `src/ui/widgets.rs` — shared components (pills, docked buttons, status row, scroll text)
- UI policy (geometry, slots, tokens, interaction, verification checklist): [docs/UI-GUIDELINES.md](docs/UI-GUIDELINES.md)
- `src/platform/` — platform abstraction trait + `cfg(target_os)` implementations (macOS/Windows)
- `src/diagnostics.rs` — scenario runner for automated visual testing (`--features diagnostics`)

### Phases

1. **Phase 1 — Basic Pipeline**: Platform abstraction + global hotkey (`Ctrl+Shift+C` double-tap) → clipboard read (with copy simulation fallback) → blocking HTTP to vLLM → strip `<think>` blocks → clipboard write.
2. **Phase 2 — Async + SSE**: `tokio` runtime on separate thread, SSE streaming, request cancellation.
3. **Phase 3 — Feedback**: Toast notifications, system tray, auto-retry on 5xx.
4. **Phase 4 — Config + Templates**: TOML config, multiple prompt templates.
5. **Phase 5 — Template Cycle UI**: Alt+Tab style overlay for template selection.
6. **Phase 6 — Windows Build**: CI, end-to-end testing, distribution (platform code already exists from Phase 1).
7. **Phase 7 — Extended**: History, config hot-reload, per-template hotkeys.

## Key Crates

| Crate | Purpose |
|-------|---------|
| `eframe` / `egui` / `egui-wgpu` / `wgpu` | UI framework + GPU rendering |
| `winit` | Window management (macOS) |
| `global-hotkey` | System-wide hotkey registration |
| `arboard` | Clipboard read/write |
| `reqwest` (`rustls-tls`) | Async HTTP client |
| `tokio` | Async runtime (worker thread) |
| `serde`, `serde_json` | JSON serialization |
| `thiserror` | Typed error definitions |
| `regex` | Think-block stripping |
| `tracing` | Structured logging |
| `tray-icon` | System tray (Windows) |
| `png` | PNG encoding (clipboard images, tray icon) |
| `zstd` | Font compression (build + runtime) |
| `base64` | Image base64 encoding for vision API |
| `core-graphics` | macOS: event simulation, display info |
| `windows-sys` | Windows: Win32 API bindings |

## Event-Driven Architecture

### Thread model

- **Main thread**: eframe event loop (`OverlayApp::update()`), egui rendering.
- **Coordinator thread**: blocks on `hotkey_rx.recv()`, runs `HotkeyDetector` tap detection, sends `TapEvent` to UI via channel + `ctx.request_repaint()`. Double-tap polling uses `recv_timeout(50ms)`.
- **Worker thread**: tokio single-threaded runtime, processes `WorkerCommand` → LLM API → `WorkerResponse`.

### Repaint model

The UI uses an **event-driven** repaint model to minimize idle CPU/GPU usage:

- **Hidden/Result/Error states**: No periodic `request_repaint_after()` — eframe sleeps until an external event arrives (hotkey, worker response, tray menu).
- **Processing state**: `request_repaint()` every frame for spinner animation and SSE streaming updates.
- **Important**: Avoid calling `send_viewport_cmd()` in loops — it internally triggers `request_repaint()`, overriding any throttle.

## Known Issues / Improvement Needed

- **macOS fullscreen overlay**: The overlay cannot appear over fullscreen apps (apps that create their own dedicated Space). `MoveToActiveSpace` + `FullScreenAuxiliary` + `CanJoinAllSpaces` + higher window levels (101) were all tried without success. DeepL achieves this — likely uses `NSPanel` or a private API. Needs further investigation.

## API Integration

Two provider flavors, selected by `[api].provider` (`CLIP_LLM_PROVIDER`). Several
backends can coexist as `[[models]]` profiles (`config::Config::model_specs` →
`LlmClient::for_spec` each; the worker holds them in a `ModelPool` and switches
on `WorkerCommand::SelectModel`, re-probing capabilities per profile):

### `openai` (default)

- Endpoint: OpenAI-compatible `/v1/chat/completions`
- Response parsing: extract `choices[0].message.content`
- Think-block stripping: regex `(?s)<think>.*?</think>`, trim whitespace after
- Images (both flavors): the user text becomes `<metadata>` (attachment count; per image the source size and, when reduced, the sent size) followed by the clipboard text inside `<content>…</content>`, then one image part per image; text-only requests are untouched. The shared preamble (`DEFAULT_PROMPT_PREAMBLE`) declares that layout so Transcribe/Summarize never reproduce the block or the tags — keep `structured_user_text` and the preamble sentence in sync. Measured 2026-09-06: a per-image marker text part next to each image was copied into Transcribe output by gemma-4-e2b; the metadata block was not. `scripts/prompt_vectors.json` I-001..I-003 check for leaks with `scripts/fixtures/chart.png`
- Thinking control (No Think) is probed per profile by **effect**, not acceptance: `reasoning_effort:"none"` → `chat_template_kwargs.enable_thinking=false` → `/no_think`, keeping the first whose reply shows no reasoning (`reasoning_tokens`, `reasoning_content`, inline `<think>`). LM Studio accepts unknown fields with 200 and keeps thinking, which is why acceptance was never enough. `thinking_control` in a profile forces the method.
- SSE (Phase 2+): parse `data: {...}` lines, accumulate `choices[0].delta.content`, finalize on `[DONE]`
- Revision rounds (both flavors): `system` and the content turn are byte-identical to the base request (prefix cache), then per round an `assistant` turn (the reply being revised) and a `user` turn (the instruction inside `[prompt].revision`, a fixed operator frame). Only the last `REVISION_WINDOW` rounds are replayed. The frame is in the user turn on purpose: a system-prompt clause made models treat request-shaped content as instructions (gemma T-006, grok G-004) — do not move it.

### `grok-oauth`

- Endpoint: xAI Responses API `https://api.x.ai/v1/responses` (the OAuth surface rejects `/chat/completions`)
- Auth: piggybacks the official Grok CLI's OAuth session (`~/.grok/auth.json`); tokens auto-refresh via `https://auth.x.ai/oauth2/token` and rotated tokens are written back to the store (xAI rotates the refresh token on every use, so write-back is required to keep the CLI session alive)
- Request shape: system prompt goes in top-level `instructions` (system items inside `input` are rejected); `store: false`; never request `reasoning.encrypted_content`
- SSE: typed events, no `[DONE]` — `response.output_text.delta` (content), `response.reasoning_*_text.delta` (reasoning), terminal `response.completed`/`response.incomplete` (mapped in `SseParser` onto the same `SseEvent` vocabulary, token cap → `"length"`)
- Images: `input_image` parts carrying a `data:` URI as a plain string (not the `{url}` object of chat/completions), after the `input_text` part; vision is probed like the chat flavor, with `max_output_tokens` at the schema minimum of 16 so the probe cannot 400 on the cap itself
