# clip-llm

System-wide LLM clipboard assistant. Captures text via global hotkey, sends it to a vLLM server, and writes the response back to the clipboard.

## Features

- **Global hotkey** — `Ctrl+Shift+C` single-tap (read clipboard) / double-tap (copy selection + auto-paste result back)
- **Translate / Rephrase / Summarize** — three processing modes with per-mode response caching
- **Rephrase parameters** — style (Correct / Casual / Formal / Business / Technical) and length (Terse / Brief / Same / Detailed / Full)
- **Vision support** — paste images from clipboard for summarization via multimodal API
- **Thinking mode** — toggle Think / NoThink per mode; model capability auto-detected at startup
- **Floating overlay** — draggable popup with streaming response, proximity-fade action button
- **OpenAI-compatible API** — works with vLLM or any `/v1/chat/completions` endpoint
- **Single binary, cross-platform** — macOS & Windows 11, no runtime dependencies

## Install

Download the latest build from the [Releases](../../releases) page.

**macOS** — `clip-llm-macos-arm64.app.zip` (Apple Silicon):
1. Unzip and move `clip-llm.app` wherever you like.
2. It is ad-hoc signed, so on first launch **right-click → Open** (or run
   `xattr -dr com.apple.quarantine clip-llm.app`) to get past Gatekeeper.
3. Grant **System Settings → Privacy & Security → Accessibility** — required to
   simulate Cmd+C/Cmd+V. The app is a menu-bar agent (no Dock icon); quit from
   its menu-bar icon.

**Windows 11** — `clip-llm-windows-x64.exe`: run the portable `.exe`; no install,
no special permissions.

Settings come from a `config.toml` next to the executable (inside
`clip-llm.app/Contents/MacOS/` on macOS) or `CLIP_LLM_*` env vars — see
[Configuration](#configuration).

## Roadmap

- [x] Phase 1 — Basic Pipeline
- [x] Phase 2 — Async API + SSE Streaming
- [ ] Phase 3 — Status Feedback + System Tray (partial: Windows + macOS tray + friendly errors done; no toast/retry)
- [ ] Phase 4 — Config File + Multiple Templates (partial: TOML config for prompts + API settings done)
- [ ] Phase 5 — Template Cycle Selection UI
- [x] Phase 6 — Windows Build & Distribution (partial: no CI/E2E tests)
- [ ] Phase 7 — Extended Features

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CLIP_LLM_API_ENDPOINT` | `http://localhost:8000/v1` | LLM API base URL |
| `CLIP_LLM_MODEL` | `MiniMaxAI/MiniMax-M2.5` | Model name for chat completions |
| `CLIP_LLM_API_KEY` | *(none)* | Bearer token for API auth (optional) |
| `CLIP_LLM_CUSTOM_HEADERS` | *(none)* | Custom HTTP headers, comma-separated `Key:Value` pairs (e.g. `X-Dep-Ticket:abc,User-Id:u1`) |
| `CLIP_LLM_NO_STREAM` | *(unset)* | Disable SSE streaming when set |
| `CLIP_LLM_CONFIG` | *(unset)* | Path to the config TOML (overrides the `config.toml`-next-to-executable lookup) |
| `RUST_LOG` | `clip_llm=info` | Log level filter ([`tracing_subscriber::EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)) |
| `DIAG_MOCK` | *(unset)* | Use mock LLM responses (requires `--features diagnostics`) |

The connection variables (`CLIP_LLM_API_ENDPOINT`, `CLIP_LLM_MODEL`,
`CLIP_LLM_API_KEY`, `CLIP_LLM_CUSTOM_HEADERS`) can also be set in `config.toml`
under `[api]`, and the environment variable wins when both are present. Streaming
is `[api].streaming = true/false`; `CLIP_LLM_NO_STREAM`, when set, forces it off
(there is no environment variable that forces streaming on).

See [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) for detailed specifications.

## Configuration

Settings can be supplied from an external TOML file without rebuilding. At
startup clip-llm reads, in order of precedence:

1. the path in `CLIP_LLM_CONFIG`, if set;
2. otherwise `config.toml` next to the executable.

If no file is found — or it is malformed — the built-in defaults are used. Every
key is optional; you only specify what you want to change. The file holds these
kinds of settings:

- **`[api]`** — connection settings (endpoint, model, API key, custom headers,
  streaming). Each mirrors a `CLIP_LLM_*` environment variable; the precedence is
  **env var > config file > built-in default**. (Exception: `CLIP_LLM_NO_STREAM`
  only forces streaming off — it cannot force `streaming = false` back on.)
- **`[generation]`** — request parameters (`temperature`, `max_tokens`,
  `request_timeout_secs`, `initial_response_timeout_secs`). No
  environment-variable equivalent: **config file > built-in default**.
  Transient request failures (connection errors, timeouts, HTTP 5xx) are
  retried once automatically before reporting an error.
- **`[hotkey]`** — `double_tap_timeout_ms`, the single/double-tap detection
  window in milliseconds (default `500`). Lower it (e.g. `300`) to shorten the
  silent wait before a single-tap resolves. No environment-variable equivalent.
- **`[ui]`** — `single_tap_pinned` / `double_tap_pinned` (both default `false`):
  whether a result starts pinned (stays open on focus loss instead of auto-hiding).
  A single-tap result is not auto-copied to the clipboard, so set
  `single_tap_pinned = true` to keep it from disappearing on focus change.
- **prompts** — per-mode system prompts, with placeholders substituted at runtime:
  - `{primary_lang}` / `{secondary_lang}` — in the `[translate]` prompt
  - `{primary_lang}` — in the `[summarize]` prompts (summaries are primary-language only)
  - `{style}` / `{length}` — only in `[rephrase].base`

See [config.example.toml](config.example.toml) for the full schema and examples.

## Inspired By

- [DeepL](https://www.deepl.com/) — hotkey-triggered floating overlay UX
- [PowerToys Advanced Paste](https://learn.microsoft.com/en-us/windows/powertoys/advanced-paste) — AI-powered clipboard transformation pipeline
