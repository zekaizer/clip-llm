use eframe::egui;

use super::state_machine::{CaptureSource, OverlayState};
use crate::{ProcessMode, RephraseLength, RephraseParams, RephraseStyle, ThinkingMode};

const OVERLAY_WIDTH: f32 = 480.0;
const MAX_RESULT_HEIGHT: f32 = 260.0;
/// Space around the frame for shadow rendering.
const SHADOW_PAD: f32 = 20.0;
/// Accent color for selected tab underlines.
fn accent_color() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(108, 166, 255, 200)
}
/// Dimmed accent color for hover underlines and rephrase indent line.
fn accent_color_dim() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(108, 166, 255, 80)
}
/// Accent color for the uncommitted cycling preview underline — between the
/// hover dim and the committed accent, signalling "not yet selected".
fn accent_color_preview() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(108, 166, 255, 140)
}
/// Action button size (square).
const ACTION_BTN_SIZE: f32 = 26.0;
/// Height of the shared top "status/think" row: Processing's spinner+label
/// (or locked "Thinking" header) and Result's clickable think toggle are
/// both rendered inside a row pinned to this height (see `fixed_height_row`),
/// so that row occupies the same slot regardless of which content variant is
/// showing — no shift in the text block below it across the Processing→Result
/// transition, or between the row's own content variants.
const TOP_ROW_HEIGHT: f32 = 24.0;
/// Height of the shared bottom "controls" row: Processing's Cancel button,
/// and Result's/Error's docked action buttons (sized `ACTION_BTN_SIZE`, which
/// this matches) all occupy this same slot — only the contents swap in place
/// across state transitions.
const BOTTOM_ROW_HEIGHT: f32 = ACTION_BTN_SIZE;
/// The frame's inner margin, named so the latch height math can subtract
/// exactly what this margin adds back around the measured inner content,
/// instead of duplicating "16, 14" as a second magic literal (see
/// `pinned_inner_height` in `render()`).
const FRAME_MARGIN: egui::Margin = egui::Margin::symmetric(16, 14);
/// Floor for the Result answer text's column when its budget is derived from
/// a pinned latch height (see `render_result`) — guards against a degenerate
/// near-zero or negative budget if the surrounding chrome alone already
/// consumes most/all of the latch.
const MIN_RESULT_TEXT_HEIGHT: f32 = 24.0;

/// Streaming and think-block display state for Processing/Result rendering.
pub struct StreamingState<'a> {
    pub text: &'a str,
    pub think_started: bool,
    pub think_content: Option<&'a str>,
    pub think_expanded: bool,
    /// `Some(reason)` when the shown Result is partial (stream cut short).
    pub incomplete: Option<&'a str>,
}

/// Thinking mode state for UI rendering.
pub struct ThinkingState {
    pub mode: ThinkingMode,
    pub supported: bool,
}

/// Action requested by the overlay UI.
pub enum OverlayAction {
    None,
    Close,
    Cancel,
    StartDrag,
    SwitchMode(ProcessMode),
    ToggleThink,
    ChangeRephraseStyle(RephraseStyle),
    ChangeRephraseLength(RephraseLength),
    ChangeThinkingMode(ThinkingMode),
    CopyToClipboard,
    PasteReplace,
    TogglePin,
    Retry,
    /// Copy the raw request/response debug snapshot to the clipboard.
    CopyDebug,
}

pub struct OverlayOutput {
    pub action: OverlayAction,
    /// Desired viewport size based on rendered content.
    pub desired_size: Option<egui::Vec2>,
    /// Raw content size before shadow padding. Used by diagnostics, and by
    /// the adapter to latch the Result/Error minimum height at a
    /// Processing→Result/Error transition (see `OverlayApp::result_latch`).
    pub content_size: Option<egui::Vec2>,
}

/// Render the overlay panel. Returns action and desired viewport size.
#[allow(clippy::too_many_arguments)]
pub fn render(
    state: &OverlayState,
    mode: ProcessMode,
    streaming: StreamingState<'_>,
    available_modes: &[ProcessMode],
    preview_mode: Option<ProcessMode>,
    picking_text: Option<&str>,
    rephrase_params: RephraseParams,
    thinking: ThinkingState,
    pinned: bool,
    auto_copy: bool,
    source: CaptureSource,
    copy_confirmed: bool,
    elapsed: Option<std::time::Duration>,
    debug_available: bool,
    // Compact completion summary ("✓ 2.4s · 850 tokens") shown in Result's
    // bottom row — the same slot Processing's spinner+elapsed+Cancel row
    // occupies (see `TOP_ROW_HEIGHT`/`BOTTOM_ROW_HEIGHT`), filling what would
    // otherwise be empty space left by those controls disappearing. `None`
    // when no completion data is available (e.g. a cached/instant result —
    // see `format_completion_status` in `mod.rs`).
    completion_status: Option<String>,
    // Floor for the Result/Error content height, latched by the adapter from
    // the last Processing frame's rendered content (see
    // `OverlayApp::result_latch`) so the final answer never renders shorter
    // than the last streaming frame — the fix for the visible
    // Processing→Result resize jump. `None` outside Result/Error, or when no
    // latch is active (falls back to normal auto-sizing). A floor only:
    // content taller than the latch grows the window naturally, up to
    // `MAX_RESULT_HEIGHT` for the answer text (see `render_result`).
    min_result_height: Option<f32>,
    ctx: &egui::Context,
) -> OverlayOutput {
    if matches!(state, OverlayState::Hidden) {
        return OverlayOutput {
            action: OverlayAction::None,
            desired_size: None,
            content_size: None,
        };
    }

    let mut action = OverlayAction::None;

    let frame = egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(30, 30, 30, 230))
        .stroke(egui::Stroke::NONE)
        .corner_radius(12)
        .inner_margin(FRAME_MARGIN)
        .shadow(egui::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: egui::Color32::from_black_alpha(100),
        });

    // --- egui Area sizing fix ---
    // egui::Area stores the previous frame's content min_size and uses it as
    // the next frame's max_rect.  Two things conspire to keep the overlay tiny:
    //
    //  1. With constrain=true (default), the *initial* sizing pass caps the
    //     Area to the viewport, which starts at the small initial window size.
    //  2. When transitioning from a short state (Processing) to a tall one
    //     (Result), the Area's max_rect is still sized for the short state,
    //     starving the ScrollArea of vertical space.
    //
    // Fix (a): constrain(false) — lets the initial sizing pass use a large
    //          default size instead of the viewport.
    // Fix (b): OverlayApp::update() calls reset_areas() on state transitions,
    //          clearing the stale stored size so the Area re-measures fresh.

    // Offset the frame so shadow renders evenly on all sides.
    let area_resp = egui::Area::new("overlay".into())
        .fixed_pos(egui::pos2(SHADOW_PAD, SHADOW_PAD))
        .constrain(false) // Fix (a): see above
        .sense(egui::Sense::drag())
        .show(ctx, |ui| {
            frame.show(ui, |ui| {
                ui.set_width(OVERLAY_WIDTH);
                let content_top = ui.cursor().top();

                // `min_result_height` (the latch) is the Frame's OUTER size —
                // `content_size` in `OverlayOutput`, which includes this
                // margin — so convert it to a target for the *inner* content
                // ui by subtracting the margin back out. Applying the raw
                // (un-adjusted) value here would inflate every pinned
                // Result/Error by the margin on top of the actual latch.
                let pinned_inner_height = min_result_height
                    .map(|h| (h - (FRAME_MARGIN.top as f32 + FRAME_MARGIN.bottom as f32)).max(0.0));

                // Floor: never render Result/Error shorter than the latch.
                // This MUST run here, at the true top of the whole inner ui
                // (before anything else is drawn) — `Ui::set_min_height`
                // reserves space measured from the *current cursor position*,
                // not from the ui's start, so calling it after some content
                // is already drawn (e.g. inside render_result, after the
                // text) would add that much space on TOP of what's already
                // used instead of acting as a floor for the total. This is
                // what makes `desired_size` naturally >= the last streaming
                // frame's size; content taller than the latch grows past it
                // (the latch is a floor, not a cap — see `render_result`).
                if let Some(h) = pinned_inner_height
                    && matches!(state, OverlayState::Result(_) | OverlayState::Error(_))
                {
                    ui.set_min_height(h);
                }

                // Picking overlay (hold-to-cycle, before commit). Show the mode
                // tabs from the start so the user sees and cycles the mode, and
                // the content area shows the data to be processed when available
                // (single-tap clipboard). The double-tap selection is captured on
                // release, so it shows a spinner until then. Content type is not
                // yet known, so all modes are offered; image-only is reconciled on
                // capture in on_content_ready.
                if matches!(state, OverlayState::Capturing) {
                    render_tab_bar(
                        ui, mode, ProcessMode::display_order(),
                        thinking, pinned, preview_mode,
                        &mut action,
                    );
                    ui.add_space(4.0);
                    ui.add(egui::Separator::default().spacing(4.0));
                    ui.add_space(4.0);
                    render_capturing(ui, picking_text, source, elapsed, &mut action);
                    return;
                }

                render_tab_bar(
                    ui, mode, available_modes,
                    thinking, pinned, preview_mode,
                    &mut action,
                );

                // Rephrase parameter rows (style + length), shown when Rephrase is active.
                if mode == ProcessMode::Rephrase && !matches!(state, OverlayState::Hidden) {
                    ui.add_space(4.0);
                    render_rephrase_params(ui, rephrase_params, &mut action);
                }

                // Separator between tab bar / params and content.
                ui.add_space(4.0);
                ui.add(egui::Separator::default().spacing(4.0));
                ui.add_space(4.0);

                match state {
                    OverlayState::Processing => {
                        render_processing(ui, mode, streaming.text, streaming.think_started, elapsed, &mut action);
                    }
                    OverlayState::Result(text) => {
                        render_result(
                            ui,
                            mode,
                            text,
                            streaming.think_content,
                            streaming.think_expanded,
                            auto_copy,
                            copy_confirmed,
                            streaming.incomplete,
                            debug_available,
                            source,
                            completion_status.as_deref(),
                            content_top,
                            pinned_inner_height,
                            &mut action,
                        );
                    }
                    OverlayState::Error(msg) => {
                        // Retry needs loaded content; a capture failure leaves
                        // none (available_modes is empty), so hide the button.
                        // The height floor for a latched Error is already
                        // applied above (at the true top of the ui) — Error
                        // has no adjustable scrollable content to cap, so
                        // that floor is this state's entire pinning story.
                        render_error(ui, msg, !available_modes.is_empty(), debug_available, &mut action);
                    }
                    // Hidden returns early at the top of render(); Capturing is handled above.
                    OverlayState::Hidden | OverlayState::Capturing => unreachable!(),
                }
            });
        });

    // Drag the OS window when the user drags the overlay area.
    if area_resp.response.drag_started() {
        action = OverlayAction::StartDrag;
    }

    // Close on Escape key.
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        action = OverlayAction::Close;
    }

    // Keyboard actions in the Result state (#52). Enter triggers the primary
    // action — paste-replace for double-tap, copy for single-tap — mirroring
    // the docked action button. Cmd/Ctrl+C copies the full result, but only
    // when no text is selected: egui's label selection handles its own copy,
    // and overwriting it would clobber a deliberate partial-text copy.
    if matches!(state, OverlayState::Result(_)) {
        if ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
            action = if auto_copy {
                OverlayAction::PasteReplace
            } else {
                OverlayAction::CopyToClipboard
            };
        }
        let copy_pressed =
            ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        if copy_pressed {
            let has_selection = ctx
                .plugin_opt::<egui::text_selection::LabelSelectionState>()
                .is_some_and(|handle| handle.lock().has_selection());
            if !has_selection {
                action = OverlayAction::CopyToClipboard;
            }
        }
    }

    // Viewport = content + shadow padding on all sides.
    let content_size = area_resp.response.rect.size();
    let desired = content_size + egui::vec2(SHADOW_PAD * 2.0, SHADOW_PAD * 2.0);

    OverlayOutput {
        action,
        desired_size: Some(desired),
        content_size: Some(content_size),
    }
}

/// Render `add_contents` inside a row whose height is pinned to exactly
/// `height` (a floor — content is never clipped, only ever centered within
/// more space than it naturally needs). Used for the shared top status/think
/// row and bottom controls row in both Processing and Result, so those rows'
/// vertical footprint is identical regardless of which content variant (or
/// which state) renders inside them — only the *contents* swap in place at
/// the Processing→Result transition, not the layout around them.
fn fixed_height_row(ui: &mut egui::Ui, height: f32, add_contents: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(height);
        add_contents(ui);
    });
}

/// Renders a vertically scrollable, word-wrapped text label with a consistent
/// style, shrinking to the content's natural height up to `max_height`.
fn render_scrollable_text(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    text: &str,
    max_height: f32,
    stick_to_bottom: bool,
) {
    egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .max_height(max_height)
        // egui's ScrollArea defaults to a 64px `min_scrolled_size` floor once
        // content needs scrolling, silently overriding a smaller max_height
        // (a `max_height` as low as e.g. 24 was still rendering at ~64px).
        // Match that floor to the same one `render_result`'s budget math
        // floors to, so a tight budget is actually honored.
        .min_scrolled_height(MIN_RESULT_TEXT_HEIGHT)
        .auto_shrink([false, true])
        .stick_to_bottom(stick_to_bottom)
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(text).color(egui::Color32::WHITE).size(15.0),
                )
                .wrap_mode(egui::TextWrapMode::Wrap),
            );
        });
}

/// Render just the clickable "▶/▼ Thinking" toggle (icon + label, unchanged
/// styling/size — #6), no expanded content. This is Result's counterpart to
/// Processing's status row and is rendered inside the shared `TOP_ROW_HEIGHT`
/// slot (see `fixed_height_row`), so it doesn't shift the text block below
/// when it replaces Processing's row at the transition. Call
/// `render_think_content` separately, *outside* that fixed slot, when
/// `expanded` — that growth is a deliberate user action, not part of the
/// pinned transition geometry.
fn render_think_toggle_header(ui: &mut egui::Ui, expanded: bool, action: &mut OverlayAction) {
    let icon = if expanded { "\u{25bc}" } else { "\u{25b6}" };
    let btn = egui::Button::new(
        egui::RichText::new(format!("{icon} Thinking"))
            .color(egui::Color32::from_gray(160))
            .size(13.0),
    )
    .fill(egui::Color32::TRANSPARENT);
    if ui.add(btn).clicked() {
        *action = OverlayAction::ToggleThink;
    }
}

/// Render the expanded think-block content (scrollable). Only shown once
/// `render_think_toggle_header`'s toggle is expanded; deliberately rendered
/// outside the fixed-height/pinned-height budget, so it's free to grow the
/// window (collapsing returns to the pinned floor, not to whatever egui
/// would otherwise auto-measure).
fn render_think_content(ui: &mut egui::Ui, content: &str) {
    egui::ScrollArea::vertical()
        .id_salt("think_content")
        .max_height(120.0)
        .auto_shrink([false, true])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(content)
                        .color(egui::Color32::from_gray(130))
                        .size(13.0),
                )
                .wrap_mode(egui::TextWrapMode::Wrap),
            );
        });
}

/// Small dim label showing how long the current request has been processing.
/// Helps the user distinguish slow generation (especially a long thinking phase)
/// from a stall.
fn render_elapsed_label(ui: &mut egui::Ui, elapsed: Option<std::time::Duration>) {
    if let Some(d) = elapsed {
        ui.label(
            egui::RichText::new(format!("{:.1}s", d.as_secs_f32()))
                .color(egui::Color32::from_gray(120))
                .size(12.0),
        );
    }
}

/// Render the "Cancel" button shown while a capture or LLM request is in
/// flight (Capturing / Processing states), setting `action` to
/// `OverlayAction::Cancel` when clicked.
fn render_cancel_button(ui: &mut egui::Ui, action: &mut OverlayAction) {
    let cancel_btn = egui::Button::new(
        egui::RichText::new("Cancel")
            .size(12.0)
            .color(egui::Color32::from_rgb(255, 140, 140)),
    )
    .fill(egui::Color32::from_rgba_unmultiplied(80, 30, 30, 180))
    .corner_radius(6.0);
    if ui.add(cancel_btn).clicked() {
        *action = OverlayAction::Cancel;
    }
}

/// Render the Capturing state: a spinner shown immediately on double-tap while the
/// selection is copied on a background thread (no content/tabs yet).
fn render_capturing(
    ui: &mut egui::Ui,
    picking_text: Option<&str>,
    source: CaptureSource,
    elapsed: Option<std::time::Duration>,
    action: &mut OverlayAction,
) {
    if let Some(text) = picking_text {
        // Single-tap picking: the clipboard content has arrived, so show the
        // data that will be processed in the chosen mode on release.
        render_scrollable_text(ui, "picking", text, MAX_RESULT_HEIGHT, false);
    } else {
        // Content not yet available — double-tap captures the selection on
        // modifier release (copy simulation needs the modifiers up) and the
        // single-tap clipboard read runs on a background thread (#38); until
        // then show a spinner with a source-appropriate label.
        ui.horizontal(|ui| {
            ui.spinner();
            let label = match source {
                CaptureSource::Selection => "Copying selection...",
                CaptureSource::Clipboard => "Reading clipboard...",
            };
            ui.label(
                egui::RichText::new(label)
                    .color(egui::Color32::WHITE)
                    .size(15.0),
            );
            render_elapsed_label(ui, elapsed);
        });
    }
    ui.add_space(4.0);
    render_cancel_button(ui, action);
}

fn render_processing(
    ui: &mut egui::Ui,
    mode: ProcessMode,
    streaming_text: &str,
    think_started: bool,
    elapsed: Option<std::time::Duration>,
    action: &mut OverlayAction,
) {
    // Top row: shared slot with Result's think toggle (see `TOP_ROW_HEIGHT`)
    // — whichever of these three variants is showing on the last Processing
    // frame, it occupies the same height as Result's row that replaces it.
    fixed_height_row(ui, TOP_ROW_HEIGHT, |ui| {
        if think_started && streaming_text.is_empty() {
            // Think block in progress, no visible output yet.
            ui.spinner();
            ui.label(
                egui::RichText::new("Thinking...")
                    .color(egui::Color32::from_gray(160))
                    .size(15.0),
            );
            render_elapsed_label(ui, elapsed);
        } else if think_started {
            // Think done, answer streaming: show locked collapsed header.
            ui.label(
                egui::RichText::new("\u{25b6} Thinking")
                    .color(egui::Color32::from_gray(100))
                    .size(13.0),
            );
            render_elapsed_label(ui, elapsed);
        } else {
            ui.spinner();
            ui.label(
                egui::RichText::new(mode.processing_label())
                    .color(egui::Color32::WHITE)
                    .size(15.0),
            );
            render_elapsed_label(ui, elapsed);
        }
    });
    if !streaming_text.is_empty() {
        ui.add_space(4.0);
        render_scrollable_text(ui, ("streaming", mode), streaming_text, MAX_RESULT_HEIGHT, true);
    }
    ui.add_space(4.0);
    // Bottom row: shared slot with Result's reserved action-button space (see
    // `BOTTOM_ROW_HEIGHT`).
    fixed_height_row(ui, BOTTOM_ROW_HEIGHT, |ui| {
        render_cancel_button(ui, action);
    });
}

fn render_error(
    ui: &mut egui::Ui,
    message: &str,
    can_retry: bool,
    debug_available: bool,
    action: &mut OverlayAction,
) {
    ui.label(
        egui::RichText::new(format!("Error: {message}"))
            .color(egui::Color32::from_rgb(255, 100, 100))
            .size(14.0),
    );
    // Bottom controls row, same slot and docked-button style as Result's (see
    // `BOTTOM_ROW_HEIGHT`): retry at the far right, copy-debug to its left —
    // matching their relative order in Result, which only adds the primary
    // copy/paste button after them.
    if can_retry || debug_available {
        ui.add_space(4.0);
        fixed_height_row(ui, BOTTOM_ROW_HEIGHT, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if can_retry && docked_action_button(ui, "\u{21bb}", "Retry") {
                    *action = OverlayAction::Retry;
                }
                if debug_available
                    && docked_action_button(
                        ui,
                        "\u{1f50d}",
                        "Copy the raw request + response to the clipboard",
                    )
                {
                    *action = OverlayAction::CopyDebug;
                }
            });
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn render_result(
    ui: &mut egui::Ui,
    mode: ProcessMode,
    text: &str,
    think_content: Option<&str>,
    think_expanded: bool,
    auto_copy: bool,
    copy_confirmed: bool,
    incomplete: Option<&str>,
    debug_available: bool,
    source: CaptureSource,
    // Compact completion summary for the bottom row (see the doc comment on
    // `render()`'s `completion_status` parameter, which this is threaded
    // from).
    completion_status: Option<&str>,
    // Top of the whole inner content ui (captured in `render()` right after
    // `ui.set_width`, *before* the tab bar/separator) — the reference point
    // for measuring how much of `pinned_inner_height` has already been used
    // by the time the answer text is about to render, so the ScrollArea
    // budget below accounts for the tab bar/separator/rephrase params too,
    // not just this function's own rows.
    content_top: f32,
    // Target inner-content height latched from the last Processing frame
    // (already margin-adjusted by `render()`, which also applies this as a
    // floor at the true top of the ui — see `pinned_inner_height` there).
    // `None` when no latch is active (normal auto-sizing via
    // `MAX_RESULT_HEIGHT`). A FLOOR while collapsed: the answer text column
    // pads up to the leftover latched budget when shorter, and grows the
    // window naturally (up to `MAX_RESULT_HEIGHT`) when taller. Ignored
    // while `think_expanded`, which is free to grow the window; collapsing
    // again returns to at least the latched height.
    pinned_inner_height: Option<f32>,
    action: &mut OverlayAction,
) {
    // Truncation / interruption banner at the very top: the partial reply is
    // shown below, so tell the user it is incomplete and why (#65).
    if let Some(reason) = incomplete {
        ui.label(
            egui::RichText::new(format!("\u{26a0} Incomplete — {reason}"))
                .color(egui::Color32::from_rgb(255, 180, 60))
                .size(13.0),
        );
        ui.add_space(4.0);
    }

    // Top row: shared slot with Processing's status/thinking row (see
    // `TOP_ROW_HEIGHT`) — but only when there's an actual think toggle to
    // show. Reserving this row unconditionally (even blank) read as an empty
    // hole above the text, which is worse than the row's height differing
    // from Processing's for the (very common) non-thinking case; a plain
    // result's text instead starts right under the separator.
    if think_content.is_some() {
        fixed_height_row(ui, TOP_ROW_HEIGHT, |ui| {
            render_think_toggle_header(ui, think_expanded, action);
        });
    }
    // Expanded think content is deliberate, user-triggered growth — kept
    // outside the fixed slot above (unaffected styling/size — #6).
    if think_expanded && let Some(content) = think_content {
        render_think_content(ui, content);
    }
    ui.add_space(4.0);

    // Answer text: always auto-sizes up to MAX_RESULT_HEIGHT. With a latch
    // active (and Think collapsed), the leftover latched budget additionally
    // acts as a FLOOR on the text column: a short answer still owns the
    // latched space (no empty gap above the bottom row, no shrink-jump at
    // the Processing→Result seam), while a taller answer is free to grow the
    // window to its natural size — capping growth at the latch too left the
    // window shorter than its content whenever the last Processing frame
    // undershot the final answer (thinking-only streams, cached/fast
    // responses, post-processed text taller than the streamed preview).
    // `used_before_text` is measured (not estimated), so it correctly
    // accounts for the optional incomplete banner above; `reserved_after_text`
    // is the fixed add_space + bottom row that always follows,
    // unconditionally.
    match (pinned_inner_height, think_expanded) {
        (Some(target), false) => {
            let used_before_text = ui.cursor().top() - content_top;
            let reserved_after_text = 4.0 + BOTTOM_ROW_HEIGHT;
            let floor =
                (target - used_before_text - reserved_after_text).max(MIN_RESULT_TEXT_HEIGHT);
            // `set_min_height` reserves space from the current cursor, so
            // applied here (right before the text, inside its own scope) it
            // floors exactly the text column: shorter content pads up to the
            // latch, taller content grows past it up to MAX_RESULT_HEIGHT.
            ui.scope(|ui| {
                ui.set_min_height(floor);
                render_scrollable_text(ui, ("result", mode), text, MAX_RESULT_HEIGHT, false);
            });
        }
        _ => render_scrollable_text(ui, ("result", mode), text, MAX_RESULT_HEIGHT, false),
    }

    // Bottom row: shared slot with Processing's Cancel-button row (see
    // `BOTTOM_ROW_HEIGHT`): the passive completion summary on the left, the
    // docked action buttons right-aligned in the otherwise-empty right side —
    // "controls swap in place" the way the top row already does.
    ui.add_space(4.0);
    fixed_height_row(ui, BOTTOM_ROW_HEIGHT, |ui| {
        render_source_badge(ui, source);
        if let Some(status) = completion_status {
            ui.label(
                egui::RichText::new(status).color(egui::Color32::from_gray(120)).size(12.0),
            );
        }
        // right_to_left: the first button rendered lands at the far right, so
        // this reads in reverse visual order — primary action at the edge,
        // then retry, then copy-debug.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Primary: auto_copy (double-tap) = paste/replace (↩);
            // otherwise copy (📋), with ✓ confirming a just-done copy (#16a).
            let (icon, tip) = if auto_copy {
                ("\u{21a9}", "Paste over the selection")
            } else if copy_confirmed {
                ("\u{2713}", "Copied")
            } else {
                ("\u{1f4cb}", "Copy to clipboard")
            };
            if docked_action_button(ui, icon, tip) {
                *action = if auto_copy {
                    OverlayAction::PasteReplace
                } else {
                    OverlayAction::CopyToClipboard
                };
            }
            if docked_action_button(ui, "\u{21bb}", "Retry") {
                *action = OverlayAction::Retry;
            }
            // Copy-debug (🔍): copies the raw request + response snapshot.
            // Shown only when a capture exists for this result.
            if debug_available
                && docked_action_button(
                    ui,
                    "\u{1f50d}",
                    "Copy the raw request + response to the clipboard",
                )
            {
                *action = OverlayAction::CopyDebug;
            }
        });
    });
    // The floor for "never shorter than the latch" is already applied once,
    // at the true top of the whole inner ui, in `render()` — see the comment
    // there on why it can't be (re)applied here instead (`Ui::set_min_height`
    // reserves space measured from the *current* cursor, not from the ui's
    // start, so calling it this late would add phantom extra height on top
    // of everything already drawn rather than act as a floor for the total).
}

/// Compact source badge in Result's bottom row — where the content came from:
/// selection (double-tap) vs clipboard (single-tap). Makes a slow double-tap
/// that resolved to a single-tap — sending stale clipboard content — visibly
/// different (#50). Icon-only to keep the row compact; the tooltip spells it out.
fn render_source_badge(ui: &mut egui::Ui, source: CaptureSource) {
    let (icon, tip) = match source {
        CaptureSource::Selection => ("\u{2702}", "Source: selection (double-tap)"),
        CaptureSource::Clipboard => ("\u{1f4cb}", "Source: clipboard (single-tap)"),
    };
    ui.label(
        egui::RichText::new(icon)
            .size(12.0)
            .color(egui::Color32::from_gray(120)),
    )
    .on_hover_text(tip);
}

/// Docked action button for the bottom controls row: a fixed
/// [`ACTION_BTN_SIZE`] square, always visible in a subdued tone that
/// brightens on hover. Returns true when clicked.
fn docked_action_button(ui: &mut egui::Ui, icon: &str, tooltip: &str) -> bool {
    let hovered = ui
        .ctx()
        .read_response(ui.next_auto_id())
        .is_some_and(|r| r.hovered());
    let (fg, bg_alpha) = if hovered {
        (egui::Color32::WHITE, 200)
    } else {
        (egui::Color32::from_gray(170), 90)
    };
    let btn = egui::Button::new(egui::RichText::new(icon).size(14.0).color(fg))
        .min_size(egui::vec2(ACTION_BTN_SIZE, ACTION_BTN_SIZE))
        .fill(egui::Color32::from_rgba_unmultiplied(50, 50, 50, bg_alpha))
        .stroke(egui::Stroke::NONE)
        .corner_radius(4.0);
    ui.add(btn).on_hover_text(tooltip).clicked()
}

fn render_param_pills<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    label: &str,
    all: &[T],
    current: T,
    get_label: impl Fn(T) -> &'static str,
    make_action: impl Fn(T) -> OverlayAction,
    action: &mut OverlayAction,
) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(label)
                .color(egui::Color32::from_gray(140))
                .size(12.0),
        );
        for &item in all {
            let is_selected = item == current;
            let text = egui::RichText::new(get_label(item))
                .size(12.0)
                .color(if is_selected {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_gray(140)
                });
            let button = egui::Button::new(text)
                .fill(if is_selected {
                    egui::Color32::from_rgba_unmultiplied(60, 60, 60, 200)
                } else {
                    egui::Color32::TRANSPARENT
                })
                .corner_radius(6.0);
            if ui.add(button).clicked() && !is_selected {
                *action = make_action(item);
            }
        }
    });
}

fn render_rephrase_params(
    ui: &mut egui::Ui,
    params: RephraseParams,
    action: &mut OverlayAction,
) {
    // Capture the outer left edge before indent shifts the cursor.
    let outer_left = ui.cursor().min.x;

    let response = ui.indent(egui::Id::new("rephrase_params"), |ui| {
        render_param_pills(
            ui,
            "Style",
            RephraseStyle::ALL,
            params.style,
            |s| s.label(),
            OverlayAction::ChangeRephraseStyle,
            action,
        );
        render_param_pills(
            ui,
            "Length",
            RephraseLength::ALL,
            params.length,
            |l| l.label(),
            OverlayAction::ChangeRephraseLength,
            action,
        );
    });

    // Draw accent line on the left edge of the indented area.
    let rect = response.response.rect;
    let line_x = (outer_left + rect.left()) / 2.0;
    ui.painter().rect_filled(
        egui::Rect::from_min_size(
            egui::pos2(line_x, rect.top() + 2.0),
            egui::vec2(1.5, rect.height() - 4.0),
        ),
        0.75,
        accent_color_dim(),
    );
}

#[allow(clippy::too_many_arguments)]
fn render_tab_bar(
    ui: &mut egui::Ui,
    current: ProcessMode,
    available_modes: &[ProcessMode],
    thinking: ThinkingState,
    pinned: bool,
    preview_mode: Option<ProcessMode>,
    action: &mut OverlayAction,
) {
    // Content is loaded whenever any mode is available; when empty the overlay
    // is idle (no clipboard content) rather than restricted, so skip the tooltip.
    let has_content = !available_modes.is_empty();
    // While cycling, the highlight follows the (uncommitted) preview — the tab
    // that would be selected on modifier release — Alt+Tab style.
    let cycling = preview_mode.is_some();
    let highlight = preview_mode.unwrap_or(current);
    ui.horizontal(|ui| {
        // Four tabs plus the right cluster (source badge, thinking pills, pin,
        // close) fill the row; tighten the default spacing so they never collide.
        ui.spacing_mut().item_spacing.x = 4.0;
        // Mode tabs (left side)
        for &mode in ProcessMode::display_order() {
            let is_available = available_modes.contains(&mode);
            let is_selected = mode == highlight && is_available;

            let text = egui::RichText::new(mode.label())
                .size(13.0)
                .color(if !is_available {
                    egui::Color32::from_gray(90)
                } else if is_selected {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_gray(100)
                });

            let button = egui::Button::new(text)
                .fill(egui::Color32::TRANSPARENT)
                .stroke(egui::Stroke::NONE)
                .corner_radius(6.0);

            let response = ui.add(button);
            // Explain why a tab is disabled instead of silently swallowing the
            // click. Two directions: a text mode locked out by an image-only
            // clipboard, or an image-only mode (OCR) locked out by text.
            let response = if !is_available && has_content {
                if mode.requires_image_only() {
                    response.on_hover_text("Requires an image-only clipboard")
                } else {
                    response.on_hover_text("Requires text — image-only clipboard")
                }
            } else {
                response
            };
            let underline_color = if is_selected {
                // Distinguish the uncommitted cycling preview from the committed mode.
                if cycling {
                    Some(accent_color_preview())
                } else {
                    Some(accent_color())
                }
            } else if response.hovered() && is_available {
                Some(accent_color_dim())
            } else {
                None
            };
            if let Some(color) = underline_color {
                let rect = response.rect;
                let underline = egui::Rect::from_min_size(
                    egui::pos2(rect.left(), rect.bottom() - 2.0),
                    egui::vec2(rect.width(), 2.0),
                );
                ui.painter().rect_filled(underline, 0.0, color);
            }
            if response.clicked() && !is_selected && is_available {
                *action = OverlayAction::SwitchMode(mode);
            }
        }

        // Right side, laid out right-to-left so the close button sits in the very
        // top-right corner, with the thinking pills (if any) to its left.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Close button — always available (the overlay otherwise dismisses only
            // via Escape, and single-tap results stay open on focus loss).
            let close = egui::Button::new(
                egui::RichText::new("\u{2715}")
                    .size(13.0)
                    .color(egui::Color32::from_gray(150)),
            )
            .fill(egui::Color32::TRANSPARENT)
            .stroke(egui::Stroke::NONE)
            .corner_radius(4.0);
            if ui.add(close).on_hover_text("Close (Esc)").clicked() {
                *action = OverlayAction::Close;
            }

            // Pin button (left of close) — when lit, the overlay stays open on
            // focus loss instead of auto-hiding.
            let pin = egui::Button::new(
                egui::RichText::new("\u{1F4CC}").size(13.0).color(if pinned {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_gray(150)
                }),
            )
            .fill(if pinned {
                egui::Color32::from_rgba_unmultiplied(60, 60, 60, 200)
            } else {
                egui::Color32::TRANSPARENT
            })
            .stroke(egui::Stroke::NONE)
            .corner_radius(4.0);
            let pin_tip = if pinned {
                "Pinned — click to allow auto-hide"
            } else {
                "Pin — keep open on focus loss"
            };
            if ui.add(pin).on_hover_text(pin_tip).clicked() {
                *action = OverlayAction::TogglePin;
            }

            // Thinking pills — only when the model supports thinking control.
            if thinking.supported {
                // Render in reverse order (right-to-left layout reverses visual order)
                for &tm in ThinkingMode::ALL.iter().rev() {
                    let is_selected = tm == thinking.mode;

                    let text = egui::RichText::new(tm.label())
                        .size(11.0)
                        .color(if is_selected {
                            egui::Color32::WHITE
                        } else {
                            egui::Color32::from_gray(130)
                        });

                    let button = egui::Button::new(text)
                        .fill(if is_selected {
                            egui::Color32::from_rgba_unmultiplied(50, 50, 50, 200)
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .corner_radius(4.0);

                    if ui.add(button).clicked() && !is_selected {
                        *action = OverlayAction::ChangeThinkingMode(tm);
                    }
                }
            }

        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render `render()` once inside a headless (no window, no display —
    /// nothing pops up on screen) egui frame and return its output. Reuses
    /// the given `ctx` rather than a fresh one, so `egui::Area`'s persisted
    /// per-frame sizing memory (see the "egui Area sizing fix" comment on
    /// `render()`) carries over between calls exactly like consecutive real
    /// frames; tests mirror the app's `ResetAreas` at a state transition by
    /// calling `ctx.memory_mut(|m| m.reset_areas())` themselves (see the
    /// `ResetAreas` handling in `mod.rs`).
    fn render_headless(
        ctx: &egui::Context,
        state: &OverlayState,
        text: &str,
        min_result_height: Option<f32>,
    ) -> OverlayOutput {
        let mut output = None;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            output = Some(render(
                state,
                ProcessMode::Translate,
                StreamingState {
                    text,
                    think_started: false,
                    think_content: None,
                    think_expanded: false,
                    incomplete: None,
                },
                ProcessMode::ALL,
                None,
                None,
                RephraseParams::default(),
                ThinkingState { mode: ThinkingMode::NoThink, supported: false },
                false,
                true,
                CaptureSource::Selection,
                false,
                None,
                false,
                None,
                min_result_height,
                ctx,
            ));
        });
        output.expect("render() must run synchronously inside ctx.run's closure")
    }

    /// Render Processing repeatedly on a fresh `Context`, mirroring the real
    /// app's continuous repaint while streaming, so the Area's sizing memory
    /// has settled by the time its last `content_size` is used as the latch
    /// (an isolated single-frame render can still be mid-settle on a brand
    /// new `Context`). Returns the settled `OverlayOutput` and the `Context`
    /// so the caller can render Result on the very same `ctx` next — the
    /// realistic "transition frame" setup.
    fn settle_processing(text: &str) -> (egui::Context, OverlayOutput) {
        let ctx = egui::Context::default();
        let mut last = None;
        for _ in 0..5 {
            last = Some(render_headless(&ctx, &OverlayState::Processing, text, None));
        }
        (ctx, last.expect("looped at least once"))
    }

    /// The latch is a FLOOR, not a ceiling: a Result whose natural
    /// (unconstrained) text is *taller* than the latch must grow the window
    /// to its natural auto-size (still capped by `MAX_RESULT_HEIGHT`) instead
    /// of absorbing the extra height into the ScrollArea. Pinning growth too
    /// left the window shorter than its content whenever the last Processing
    /// frame undershot the final answer (thinking-only streams, cached/fast
    /// responses, post-processed text taller than the streamed preview).
    /// The Area reset mirrors the app's unconditional `ResetAreas` at the
    /// transition (see `execute_effects` in `mod.rs`) — without it the
    /// carried-over Area memory starves the ScrollArea and the height only
    /// crawls toward natural over many frames instead of landing in one.
    #[test]
    fn result_grows_beyond_latch_when_text_is_taller() {
        let (ctx, processing) = settle_processing("hi");
        let latch = processing.content_size.expect("Processing must report a content size").y;

        ctx.memory_mut(|m| m.reset_areas());
        let long_text = "line\n".repeat(200);
        let result =
            render_headless(&ctx, &OverlayState::Result(long_text.clone()), "", Some(latch));
        let result_height = result.content_size.expect("Result must report a content size").y;

        assert!(result_height >= latch - 0.5, "must never render shorter than the latch");
        assert!(
            result_height > latch + 50.0,
            "a Result much taller than the latch ({latch}) must grow the window \
             ({result_height}) instead of scrolling inside the latched height",
        );

        // Growth converges to the same auto-size an unlatched render produces
        // (i.e. natural height capped by MAX_RESULT_HEIGHT) — the latch only
        // ever adds height, never subtracts it.
        let unlatched_ctx = egui::Context::default();
        let mut unlatched = None;
        for _ in 0..5 {
            unlatched = Some(render_headless(
                &unlatched_ctx,
                &OverlayState::Result(long_text.clone()),
                "",
                None,
            ));
        }
        let unlatched_height = unlatched.unwrap().content_size.unwrap().y;
        assert!(
            (result_height - unlatched_height).abs() < 12.0,
            "latched growth ({result_height}) must converge to the unlatched \
             auto-size ({unlatched_height})",
        );
    }

    /// The floor side: a Result whose natural text is *shorter* than the
    /// latch must still render at exactly the latch height on the very next
    /// frame (never shrink below it). The text column's floor (`set_min_height`
    /// in `render_result`) pads up to the latched budget rather than shrinking
    /// to the short content, so no empty gap is left between the text column
    /// and the bottom row.
    #[test]
    fn result_pinned_height_matches_latch_when_text_is_shorter() {
        let (ctx, processing) =
            settle_processing("a fairly long streaming preview of the answer so far");
        let latch = processing.content_size.expect("Processing must report a content size").y;

        ctx.memory_mut(|m| m.reset_areas());
        let result = render_headless(&ctx, &OverlayState::Result("short".into()), "", Some(latch));
        let result_height = result.content_size.expect("Result must report a content size").y;

        // Same tolerance/rationale as the "taller" test above: never shorter
        // (tight), a few px of ScrollArea-internal-chrome overshoot tolerated.
        assert!(result_height >= latch - 0.5, "must never render shorter than the latch");
        assert!(
            result_height < latch + 12.0,
            "pinned Result height {result_height} must stay close to the latch {latch} \
             even though the natural text is much shorter",
        );
    }

    /// Without a latch (`min_result_height: None`), Result falls back to
    /// normal auto-sizing — a much longer text must render measurably taller
    /// than a short one, proving the cap in the tests above is really doing
    /// something (not just always producing the same size regardless).
    #[test]
    fn result_without_latch_auto_sizes_normally() {
        let ctx = egui::Context::default();
        let mut short = None;
        let mut long = None;
        let long_text = "line\n".repeat(200);
        for _ in 0..5 {
            short = Some(render_headless(&ctx, &OverlayState::Result("short".into()), "", None));
            long = Some(render_headless(&ctx, &OverlayState::Result(long_text.clone()), "", None));
        }

        let short_h = short.unwrap().content_size.unwrap().y;
        let long_h = long.unwrap().content_size.unwrap().y;
        assert!(
            long_h > short_h + 10.0,
            "without a latch, a much longer result ({long_h}) must render taller \
             than a short one ({short_h})",
        );
    }
}
