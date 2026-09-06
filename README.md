# clip-llm

System-wide LLM clipboard assistant. Captures text via global hotkey, sends it to a vLLM server, and writes the response back to the clipboard.

## Features

- **Global hotkey** — `Ctrl+Shift+C` single-tap (read clipboard) / double-tap (copy selection + auto-paste result back), hold-cycle to switch mode
- **Translate / Rephrase / Summarize / Explain / Transcribe** — five processing modes with per-mode response caching; tab order configurable (`[ui].tabs`)
- **Transcribe** — re-expresses clipboard content (text, image, or both) as structured Markdown: GFM tables, mermaid blocks (diagram type matched to what the diagram shows, with syntax-pitfall guardrails), tagged code fences, LaTeX, prose — inline SVG only as a last resort for freeform drawings. Wording is preserved verbatim; only the structure is chosen
- **Rephrase parameters** — style (Correct / Casual / Formal / Business / Technical) and length (Terse / Brief / Same / Detailed / Full)
- **Vision support** — paste images from clipboard for summarization via multimodal API (`openai` provider)
- **File clipboard** — files copied in Finder/Explorer are read directly: UTF-8 text files become the input (several files are joined under `=== name ===` headers), image files (PNG, JPEG, GIF, WebP, BMP) become images. Anything else (folders, binaries, PDF/Office, non-UTF-8 text) is refused by name instead of being sent half-read; 1 MiB per text file, 2 MiB total
- **Thinking mode** — toggle Think / NoThink per mode with configurable per-mode defaults. The knob that switches thinking off (`reasoning_effort`, `chat_template_kwargs`, `/no_think`) is probed per model profile by checking that the reply really carries no reasoning, and can be forced with `thinking_control`
- **Translation direction decided in code** — the prose of the input (comments and string literals included, identifiers and URLs excluded) decides whether it is Korean; the prompt then states "translate into English" or "translate into Korean" instead of asking the model. Fixes code with Korean comments and English text on small models coming back unchanged ([ADR-0002](docs/adr/0002-translation-direction-detected-in-code.md))
- **Dictionary lookups** — a single word ("throughput", "멱등성") gets a Markdown dictionary entry instead of a translation: `# headword`, then `**equivalent** · `transliteration` · *part of speech*` (the transliteration is the English term's sound in Hangul, a pronunciation hint after the equivalent), up to three senses, one example (`[translate].dictionary`)
- **Revision rounds** — not happy with a result? Type an instruction into the input under it ("more formal", "keep only the first sentence"; Tab focuses it) and Enter sends the reply back with that instruction; Undo (↶ / Cmd+Z) steps back a round. The instruction rides in the last user turn inside a fixed operator frame (`[prompt].revision`), the system prompt and the original text stay byte-identical, so the server's prefix cache still hits; the last 3 rounds are replayed as history
- **Floating overlay** — fixed-size panel (drag the corner grip to resize; the size is remembered) with streaming response and docked action buttons (copy/paste, retry, copy-debug). Layout and design language: [docs/UI-GUIDELINES.md](docs/UI-GUIDELINES.md)
- **Two API providers** — `openai`: vLLM or any `/v1/chat/completions` endpoint; `grok-oauth`: xAI's Responses API through the official Grok CLI's sign-in (no API key — tokens auto-refresh and write back to `~/.grok/auth.json`)
- **Model profiles** — several backends in one config (`[[models]]`); switch from the tray's *Model* submenu, the ⇄ button (or model name) under a result — which re-runs the same text on the new model — or the radio in Settings › Models, which also shows the active profile; Save keeps the active profile as the startup default (`[ui].default_model`)
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
- [x] Phase 5 — Template Cycle Selection UI
- [x] Phase 6 — Windows Build & Distribution (partial: no CI/E2E tests)
- [ ] Phase 7 — Extended Features (partial: config reload, model profiles, file clipboard; no history / per-template hotkeys)

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `CLIP_LLM_PROVIDER` | `openai` | API provider: `openai` (chat/completions) or `grok-oauth` (xAI Responses API via the Grok CLI's OAuth session) |
| `CLIP_LLM_API_ENDPOINT` | `http://localhost:8000/v1` | LLM API base URL (`grok-oauth` defaults to `https://api.x.ai/v1`) |
| `CLIP_LLM_MODEL` | `MiniMaxAI/MiniMax-M2.5` | Model name served by the endpoint |
| `CLIP_LLM_API_KEY` | *(none)* | Bearer token for API auth (unused by `grok-oauth`) |
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
key is optional; you only specify what you want to change.

The tray's **Settings…** opens an in-app panel for the everyday knobs —
model profiles (add, edit, remove, test the connection, pick the one used at
startup), language pair, startup mode, double-tap window, pin defaults and
per-mode thinking. Saving rewrites only those keys in `config.toml` (comments
and everything else stay as they were) and applies the result at once,
including a changed set of profiles. Prompts and `[generation]` stay file-only.

After editing the file by hand, pick **Reload Config** from the tray menu: prompts,
languages, `[generation]`, `[ui]` pin defaults, per-mode thinking and the model
profiles' connection settings apply at once (the tray *Status → Config* row
reports the outcome; a broken file is rejected and the previous settings stay
active). `[ui].tabs`, `[hotkey]` and `[telemetry]` still need a restart. The
file holds these kinds of settings:

- **`[api]`** — connection settings (provider, endpoint, model, API key, custom
  headers, streaming). Each mirrors a `CLIP_LLM_*` environment variable; the
  precedence is **env var > config file > built-in default**. (Exception:
  `CLIP_LLM_NO_STREAM` only forces streaming off — it cannot force
  `streaming = false` back on.) With `provider = "grok-oauth"` only `model` is
  required: sign in once with the Grok CLI (`grok`) and clip-llm reuses (and
  auto-refreshes) its session from `~/.grok/auth.json` (`auth_file` overrides
  the path).
- **`[[models]]`** — additional model profiles, each with the same keys as
  `[api]` (`provider`, `endpoint`, `model`, `api_key`, `auth_file`, `headers`)
  plus optional `name` (display label, defaults to `model`), `max_tokens`,
  `token_budget` and `thinking_control` (`auto` | `reasoning_effort` |
  `chat_template_kwargs` | `prompt_tag` | `none`) overrides. The `[api]` section is the first profile (and the
  only one the `CLIP_LLM_*` variables apply to); when `[[models]]` entries exist
  and `[api]` names no model, the profiles alone are used. The first profile is
  active at startup (or the one named by `[ui].default_model`); pick another
  from the tray *Model* submenu or click the model name under a result. A
  profile that fails to build (missing key, unknown provider) is listed as
  unavailable in the tray instead of stopping the app. Profiles can also be
  managed from the *Settings…* panel.
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
  `tabs = ["translate", "rephrase", "summarize", "explain", "transcribe"]` sets the
  tab-bar order; the first entry is the mode selected at startup (reorder-only —
  modes can't be hidden). `panel_size = [512, 380]` is the overlay size in
  points — written automatically when you drag the resize grip, removed when
  you double-click it (built-in default). `position = "cursor"` (default)
  centers the overlay on the trigger point; `"remembered"` reopens it where
  you last left it (the app writes `panel_position` for you). The Settings
  panel always reopens where it was left (`settings_position`, written on
  close; the first time it opens in the middle of the display). `zoom = 1.0`
  scales the whole UI (0.5–3.0); Cmd/Ctrl +/−/0 change it at runtime and the
  value is written back. `theme = "dark"` (default) | `"light"` | `"system"`
  picks the palette (`system` follows the OS appearance).
- **per-mode thinking** — `[translate|rephrase|summarize|explain|transcribe].thinking =
  "think" | "no_think"` overrides that mode's default thinking (built-ins:
  translate/rephrase/transcribe = `no_think`, summarize/explain = `think`); applies only
  when the connected model supports thinking control.
- **prompts** — per-mode system prompts, with placeholders substituted at runtime:
  - `{primary_lang}` / `{secondary_lang}` — in the `[translate]` prompt
  - `{primary_lang}` — in the `[summarize]` and `[explain]` prompts (both are primary-language only)
  - none in `[transcribe]` — it transcribes, so the output language is whatever the image shows
  - `{style}` / `{length}` — only in `[rephrase].base`

See [config.example.toml](config.example.toml) for the full schema and examples.

## Inspired By

- [DeepL](https://www.deepl.com/) — hotkey-triggered floating overlay UX
- [PowerToys Advanced Paste](https://learn.microsoft.com/en-us/windows/powertoys/advanced-paste) — AI-powered clipboard transformation pipeline
