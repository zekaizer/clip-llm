# clip-llm

System-wide LLM clipboard assistant. Captures text via global hotkey, sends it to a vLLM server, and writes the response back to the clipboard.

## Features

- Global hotkey trigger (`Ctrl+Shift+C` single-tap: clipboard, double-tap: copy selection)
- Clipboard read / copy simulation
- OpenAI-compatible API integration (vLLM)
- `<think>` block stripping
- Cross-platform (macOS, Windows 11)
- Single binary, no runtime dependencies

## Roadmap

- [ ] Phase 1 — Basic Pipeline
- [ ] Phase 2 — Async API + SSE Streaming
- [ ] Phase 3 — Status Feedback + System Tray
- [ ] Phase 4 — Config File + Multiple Templates
- [ ] Phase 5 — Template Cycle Selection UI
- [ ] Phase 6 — Windows Build & Distribution
- [ ] Phase 7 — Extended Features

See [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md) for detailed specifications.
