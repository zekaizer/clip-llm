# UI Guidelines

The overlay is one panel that every state (Capturing, Processing, Result,
Error) draws into. These rules keep it consistent; the code that enforces
them lives in `src/ui/theme.rs` (tokens), `src/ui/widgets.rs` (components)
and `src/ui/panel.rs` (frame + layout slots). Views in `src/ui/overlay.rs`
only compose those three — they never introduce a literal size, color or
spacing of their own.

## 1. Geometry: one fixed panel size per session

- The panel has a **fixed size** (`[ui].panel_size`, default
  `theme::size::DEFAULT_PANEL`). Content never changes the size: a one-line
  answer and a 200-line answer show in the same frame; the text scrolls.
- The user changes the size by dragging the **grip** in the bottom-right
  corner; the result is written back to `[ui].panel_size` when the drag ends.
  Double-clicking the grip restores the default and removes the key.
- The size is clamped to `theme::size::MIN_PANEL` so the header row and one
  text line plus the footer always fit.
- Placement (`[ui].position`): by default the panel is centered on the
  trigger point (the cursor) on each new trigger; `"remembered"` reopens it
  where it was last left (stored in `[ui].panel_position` on hide). Once the
  user drags or resizes it, placement is theirs until the next trigger.
- Zoom (`[ui].zoom`, Cmd/Ctrl +/−/0 at runtime): scales everything; the
  panel size stays in points, so the window grows with the zoom. Positioning
  math works in OS points (`os_window_size`), never in egui points.

Why fixed: content-driven sizing made the window jump between Processing
and Result and needed a stack of workarounds (latched heights, per-row
height matching, egui Area resets). A fixed frame removes the whole class.

Exception: the **Settings** panel is a form, not a live view — it keeps a
fixed width (`theme::size::SETTINGS_WIDTH`) and sizes its height to the
form. Long forms are split into sub-pages rather than scrolled.

## 2. Layout: header / status / body / footer

Every view fills the same four slots, top to bottom, provided by
`panel::Slots`:

| Slot | Content | Height |
|------|---------|--------|
| **Header** | Mode tabs left; thinking pills, pin, close right. Rephrase adds its parameter rows underneath. A separator closes the header. | Natural |
| **Status** | One line saying what is going on: spinner + phase + elapsed while capturing or processing; the Think toggle and completion summary (`✓ 2.4s · model`, doubles as the model switch) in Result; `✕ Request failed` in Error. | `theme::size::ROW` |
| **Body** | The one thing the state is about: picked text, streaming text, the answer, the error message. Text fills the remaining height and scrolls. | Remainder |
| **Footer** | Left: the source badge. Right: actions, primary at the far right (`↩`/`📋`, then `↻`, then `🔍`; Capturing and Processing show Cancel). | `theme::size::ACTION_BTN` |

Rules:
- Every state fills every slot (an empty slot keeps its height), so a
  transition only swaps contents — the text block never moves at
  Processing → Result, which is what made the old overlay flicker.
- Controls that exist in more than one state occupy the same slot and
  position in each.
- Nothing scrolls except the body text (and the collapsed-by-default Think
  block, capped at `theme::size::THINK_MAX_HEIGHT`). No nested scroll areas.
- Body content that could overflow (error messages included) goes through
  `Body::fill_text`, which scrolls within the remaining height.

## 3. Design language: tokens only

`theme.rs` is the single source of truth. Views use the roles below; a new
role is added to the theme, never inlined. Every color role exists in a dark
and a light palette (`[ui].theme`: `dark` default, `light`, `system`);
`theme::color::apply(ctx)` selects the frame's palette from egui's resolved
theme, so a role is always read through its function (`color::text()`), never
cached across frames.

**Type scale** (`theme::font`): `TITLE` 16 (settings title), `BODY` 15
(content: picked text, answer, error message), `LABEL` 13 (tabs, every
status-row label — so the row keeps one weight across states — Think toggle,
row labels), `CAPTION` 12 (pills, hints, buttons, elapsed time), `MICRO` 11
(section headers, thinking pills).

**Text tones** (`theme::color`): `text` content and selected controls ·
`text_soft` form labels · `text_secondary` idle interactive controls ·
`text_muted` captions, hints, unselected tabs, Think content ·
`text_disabled` unavailable controls and the idle grip.

**Semantic colors**: `accent` selection underline (`accent_preview` while
cycling, `accent_dim` on hover, `accent_fill` for selected accent pills) ·
`danger` errors and Cancel (`danger_fill` behind Cancel) · `warning`
degraded-but-running notices (retry pending, incomplete result) · `success`
confirmations.

**Surfaces**: `surface` the frame (also the color body text fades into) ·
`surface_raised` a selected neutral control · `surface_hover` hovered docked
button · `surface_subtle` idle pill or docked button · `rule` form separators
· `shadow` the frame's drop shadow.

**Spacing and shape** (`theme::space`, `theme::size`): gaps `XS` 2 / `SM` 4 /
`MD` 8 / `LG` 10; `ROW` 24 for status rows and pills; `ACTION_BTN` 26;
corner radius `RADIUS_SM` 4 for icon buttons, `RADIUS` 6 for pills and text
buttons, `FRAME_RADIUS` 12 for the panel.

## 4. Interaction

- **Escape** closes (a dropdown first, in Settings). **Enter** runs the
  primary action in Result. **Cmd/Ctrl+C** copies the whole answer unless a
  selection exists. **Cmd/Ctrl+S** saves in Settings.
- **←/→** step through the available tabs, **Cmd/Ctrl+1…5** jump to a tab
  (Result and Error). **↑/↓**, **PageUp/PageDown/Space**, **Home/End** scroll
  the body text.
- Dragging anywhere on the panel moves it; dragging the grip resizes it.
- A state with an in-flight operation always offers **Cancel**; Error always
  offers **Retry** when content exists. Actions are icon buttons with a
  tooltip; the tooltip names the shortcut where one exists.
- Focus loss hides the panel unless pinned (`📌`) or still processing.
  Error stays until dismissed the same way Result does.
- Disabled controls stay visible in `TEXT_DISABLED` with a tooltip that says
  why (never silently swallow a click).

## 5. Feedback

- Every wait shows a spinner, a label naming the phase (`Translating…`,
  `Thinking…`, `Copying selection…`) and elapsed seconds in `TEXT_MUTED`.
- Degraded states (automatic retry, truncated answer) are `WARNING`; failures
  are `DANGER` with a user-facing message (see #27) — never raw library
  errors.
- Confirmations (copied) flip the action icon to `✓` for a short moment
  instead of adding UI.

## 6. Rendering

- Event-driven repaint: only Processing repaints continuously (spinner and
  stream). See the repaint model in `CLAUDE.md`.
- Never call `send_viewport_cmd` in a loop; geometry changes go through
  `OverlayApp::update_viewport`, which sends only on change.

## 7. Verifying a UI change

Run the diagnostics runner and read the captures, not just the tests:

```bash
CLIP_LLM_API_ENDPOINT=http://127.0.0.1:1/v1 CLIP_LLM_MODEL=mock CLIP_LLM_API_KEY=mock \
DIAG_MOCK=1 cargo run --features diagnostics      # target/diagnostics/*.png
```

Checklist for a new or changed view:
1. It fills every `panel::Slots` slot (header/status/body/footer) and draws nothing outside them.
2. Every size/color/spacing is a `theme` token.
3. Controls shared with another state sit in the same slot and order.
4. Overflowing text goes through `Body::fill_text`.
5. A diagnostics scenario captures the new state or variant.
