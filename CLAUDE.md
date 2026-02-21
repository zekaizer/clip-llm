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

## Workflow

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
- `src/lib.rs` — library crate: all business logic, testable independently
- `src/platform/` — platform abstraction trait + `cfg(target_os)` implementations (macOS/Windows)

### Phases

1. **Phase 1 — Basic Pipeline**: Platform abstraction + global hotkey (`Ctrl+Shift+C` double-tap) → clipboard read (with copy simulation fallback) → blocking HTTP to vLLM → strip `<think>` blocks → clipboard write.
2. **Phase 2 — Async + SSE**: `tokio` runtime on separate thread, SSE streaming, request cancellation.
3. **Phase 3 — Feedback**: Toast notifications, system tray, auto-retry on 5xx.
4. **Phase 4 — Config + Templates**: TOML config, multiple prompt templates.
5. **Phase 5 — Template Cycle UI**: Alt+Tab style overlay for template selection.
6. **Phase 6 — Windows Build**: CI, end-to-end testing, distribution (platform code already exists from Phase 1).
7. **Phase 7 — Extended**: History, config hot-reload, per-template hotkeys.

## Key Crates

| Crate | Purpose | Phase |
|-------|---------|-------|
| `global-hotkey` | System-wide hotkey registration | 1 |
| `arboard` | Clipboard read/write | 1 |
| `reqwest` (`rustls-tls`) | HTTP client (blocking→async) | 1→2 |
| `serde`, `serde_json` | JSON serialization | 1 |
| `thiserror` | Typed error definitions | 1 |
| `regex` | Think-block stripping | 1 |
| `tracing` | Structured logging | 1 |
| `dirs` | Platform-specific config directories | 4+ |
| `tokio` | Async runtime | 2+ |
| `tray-icon` | System tray | 3+ |
| `notify-rust` | Toast notifications | 3+ |
| `toml` | Config file parsing | 4+ |

## API Integration

- Endpoint: OpenAI-compatible `/v1/chat/completions`
- Response parsing: extract `choices[0].message.content`
- Think-block stripping: regex `(?s)<think>.*?</think>`, trim whitespace after
- SSE (Phase 2+): parse `data: {...}` lines, accumulate `choices[0].delta.content`, finalize on `[DONE]`
