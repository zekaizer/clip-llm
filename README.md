# clip-llm

System-wide LLM clipboard assistant. Captures text via global hotkey, sends it to a vLLM server, and writes the response back to the clipboard.

## Features

- **Global hotkey** — `Ctrl+Shift+C` single-tap (clipboard) / double-tap (copy selection) to trigger
- **Translate / Correct / Summarize** — three processing modes, switchable via tab bar
- **Per-mode response caching** — instant tab switching without redundant API calls
- **Floating overlay** — draggable popup with streaming response display
- **OpenAI-compatible API** — works with vLLM or any `/v1/chat/completions` endpoint
- **Single binary, cross-platform** — macOS & Windows 11, no runtime dependencies

## Roadmap

- [x] Phase 1 — Basic Pipeline
- [x] Phase 2 — Async API + SSE Streaming
- [ ] Phase 3 — Status Feedback + System Tray
- [ ] Phase 4 — Config File + Multiple Templates
- [ ] Phase 5 — Template Cycle Selection UI
- [x] Phase 6 — Windows Build & Distribution (partial: no CI/tests yet)
- [ ] Phase 7 — Extended Features

See [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) for detailed specifications.
