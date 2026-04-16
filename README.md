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

## Roadmap

- [x] Phase 1 — Basic Pipeline
- [x] Phase 2 — Async API + SSE Streaming
- [ ] Phase 3 — Status Feedback + System Tray (partial: Windows tray done, no macOS tray/toast/retry)
- [ ] Phase 4 — Config File + Multiple Templates
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
| `RUST_LOG` | `clip_llm=info` | Log level filter ([`tracing_subscriber::EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)) |
| `DIAG_MOCK` | *(unset)* | Use mock LLM responses (requires `--features diagnostics`) |

See [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) for detailed specifications.

## Inspired By

- [DeepL](https://www.deepl.com/) — hotkey-triggered floating overlay UX
- [PowerToys Advanced Paste](https://learn.microsoft.com/en-us/windows/powertoys/advanced-paste) — AI-powered clipboard transformation pipeline
