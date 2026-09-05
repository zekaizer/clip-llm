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
- Placement: the panel is centered on the trigger point (the cursor) on each
  new trigger. Once the user drags or resizes it, placement is theirs until
  the next trigger.

Why fixed: content-driven sizing made the window jump between Processing
and Result and needed a stack of workarounds (latched heights, per-row
height matching, egui Area resets). A fixed frame removes the whole class.

Exception: the **Settings** panel is a form, not a live view — it keeps a
fixed width (`theme::size::SETTINGS_WIDTH`) and sizes its height to the
form. Long forms are split into sub-pages rather than scrolled.

## 2. Layout: header / body / footer

Every view fills the same three slots, top to bottom, provided by
`panel::Slots`:

| Slot | Content | Height |
|------|---------|--------|
| **Header** | Mode tabs left; thinking pills, pin, close right. Rephrase adds its parameter rows underneath. A separator closes the header. | Natural |
| **Body** | The one thing the state is about: picked text, streaming text, the answer, the error. A status row (spinner + label + elapsed, or the Think toggle) may sit at its top. Text fills the remaining height and scrolls. | Remainder |
| **Footer** | Left: passive status (source badge, completion summary, model). Right: actions, primary at the far right (`↩`/`📋`, then `↻`, then `🔍`; Processing shows Cancel). | `theme::size::ACTION_BTN` |

Rules:
- Controls that exist in more than one state occupy the same slot and
  position in each, so only the contents swap at a transition.
- Nothing scrolls except the body text (and the collapsed-by-default Think
  block, capped at `theme::size::THINK_MAX_HEIGHT`). No nested scroll areas.
- Body content that could overflow (error messages included) goes through
  `Body::fill_text`, which scrolls within the remaining height.

## 3. Design language: tokens only

`theme.rs` is the single source of truth. Views use the roles below; a new
role is added to the theme, never inlined.

**Type scale** (`theme::font`): `TITLE` 16 (settings title), `BODY` 15
(content: picked text, answer, error, status label), `LABEL` 13 (tabs,
Think toggle, row labels), `CAPTION` 12 (pills, footer status, hints,
buttons), `MICRO` 11 (section headers, thinking pills).

**Text tones** (`theme::color`): `TEXT` content and selected controls ·
`TEXT_SOFT` form labels · `TEXT_SECONDARY` idle interactive controls ·
`TEXT_MUTED` captions, hints, unselected tabs, Think content ·
`TEXT_DISABLED` unavailable controls and the idle grip.

**Semantic colors**: `ACCENT` selection underline (`ACCENT_PREVIEW` while
cycling, `ACCENT_DIM` on hover, `ACCENT_FILL` for selected accent pills) ·
`DANGER` errors and Cancel (`DANGER_FILL` behind Cancel) · `WARNING`
degraded-but-running notices (retry pending, incomplete result) · `SUCCESS`
confirmations.

**Surfaces**: `SURFACE` the frame · `SURFACE_RAISED` a selected neutral
control · `SURFACE_HOVER` hovered docked button · `SURFACE_SUBTLE` idle pill
or docked button.

**Spacing and shape** (`theme::space`, `theme::size`): gaps `XS` 2 / `SM` 4 /
`MD` 8 / `LG` 10; `ROW` 24 for status rows and pills; `ACTION_BTN` 26;
corner radius `RADIUS_SM` 4 for icon buttons, `RADIUS` 6 for pills and text
buttons, `FRAME_RADIUS` 12 for the panel.

## 4. Interaction

- **Escape** closes (a dropdown first, in Settings). **Enter** runs the
  primary action in Result. **Cmd/Ctrl+C** copies the whole answer unless a
  selection exists. **Cmd/Ctrl+S** saves in Settings.
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
1. It uses `panel::Slots` (header/body/footer) and nothing outside them.
2. Every size/color/spacing is a `theme` token.
3. Controls shared with another state sit in the same slot and order.
4. Overflowing text goes through `Body::fill_text`.
5. A diagnostics scenario captures the new state or variant.
