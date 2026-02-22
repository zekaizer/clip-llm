# Known Issues

## Windows: High CPU usage when overlay is hidden

**Status:** Workaround applied (upstream bug)
**Severity:** Medium — ~10% CPU idle on multi-core systems
**Affected:** Windows 11, eframe 0.31+ with wgpu backend

### Upstream Tracking

| Link | Description |
|------|-------------|
| [egui#5229](https://github.com/emilk/egui/issues/5229) | `Visible(false)` → `Visible(true)` broken + CPU spin |
| [egui#7776](https://github.com/emilk/egui/issues/7776) | ~16% CPU with `Visible(false)` on eframe 0.33.3 |
| [egui#7905](https://github.com/emilk/egui/pull/7905) | Fix PR (opened 2026-02-14, **not yet merged**) |
| [egui#3982](https://github.com/emilk/egui/issues/3982) | Related: minimized CPU fix (0.26.1), hidden case excluded |

### Root Cause

eframe on Windows switches to `ControlFlow::Poll` when the window is `Visible(false)`.
Windows does not send `WM_PAINT` / `RedrawRequested` to invisible windows, so the
winit event loop spins without sleeping — consuming CPU.

The mechanism in detail:

1. `ctx.request_repaint()` → `UserEvent::RequestRepaint { when: now }` via event loop proxy
2. eframe stores in `windows_next_repaint_times[viewport_id] = when`
3. `check_redraw_requests()`: `now >= when` → `ControlFlow::Poll` + `window.request_redraw()`
4. Hidden window → WM_PAINT not delivered → `RedrawRequested` never fires → entry not consumed
5. Infinite loop: Poll every iteration → event loop spin → ~10% CPU

The minimized case was fixed in eframe 0.26.1 (PR#3985), but the hidden/invisible
case remains unfixed through eframe 0.33.3. Downgrading does not help.

This explains the counterintuitive **Show < Hide CPU** pattern:
- **Show (~3%)**: `ControlFlow::Wait` — event loop blocks until an event arrives.
- **Hide (~10%)**: `ControlFlow::Poll` (bug) — event loop spins continuously.

`update()` is never called in Hidden state (no WM_PAINT), so in-update workarounds
like `thread::sleep` have no effect.

### Workaround

**Approach**: Move the window off-screen (`SetWindowPos` to -32000,-32000) instead of
hiding it. The window stays visible from winit/eframe's perspective, so `WM_PAINT` is
still delivered, repaint entries are consumed normally, and the event loop stays in
`ControlFlow::Wait` (zero CPU).

At startup, the window is created with `with_visible(false)` (winit `visible=false` →
`ControlFlow::Poll`). The first `update()` call sends `Visible(true)` to flip winit's
internal state while keeping the window off-screen.

**Show flow**: `pre_show()` → `show_no_activate()` → `SetWindowPos(cursor)` +
`ShowWindowAsync(SW_SHOWNA)` → `ctx.request_repaint()` → `update()` →
`show_and_focus_window()` + `Visible(true)`.

**Hide flow**: `hide_window()` → `move_window_offscreen()` → `SetWindowPos(-32000,-32000)`.
Window stays visible → `ControlFlow::Wait` → CPU ~0%.

**Failed approaches**:
1. `request_repaint_after(3600s)` — eframe uses `MIN(existing, new)` semantics; a future
   timestamp cannot override a past stale entry.
2. `ShowWindowAsync(SW_HIDE)` — still triggers `WM_SIZE(0,0)` → `WindowEvent::Resized` →
   `repaint: true` → same stale entry problem.

Location: `src/platform/windows.rs` (`move_window_offscreen()`), `src/ui/mod.rs` (`hide_window()`).

### Resolution Plan

Remove the workaround when upgrading to an eframe version that includes PR#7905
(expected 0.34.0+).

Breaking changes to watch when upgrading from eframe 0.31:

| Version | Breaking Change |
|---------|----------------|
| 0.32.0  | Rust edition 2024, `winapi` → `windows-sys` |
| 0.33.0  | MSRV 1.88, Plugin trait API |
