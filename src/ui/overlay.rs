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
/// Action button: distance (px) at which the button becomes fully transparent.
const ACTION_BTN_FADE_RADIUS: f32 = 80.0;
/// Action button: maximum alpha value at zero distance from cursor.
const ACTION_BTN_ALPHA_MAX: f32 = 200.0;
/// Action button size (square).
const ACTION_BTN_SIZE: f32 = 26.0;
/// Height of the shared top "status/think" row: Processing's spinner+label
/// (or locked "Thinking" header) and Result's clickable think toggle are
/// both rendered inside a row pinned to this height (see `fixed_height_row`),
/// so that row occupies the same slot regardless of which content variant is
/// showing — no shift in the text block below it across the Processing→Result
/// transition, or between the row's own content variants.
const TOP_ROW_HEIGHT: f32 = 24.0;
/// Height of the shared bottom "controls" row: Processing's Cancel button and
/// Result's reserved space for the floating action buttons (see
/// `ACTION_BTN_SIZE`, which this matches) both occupy this same slot.
const BOTTOM_ROW_HEIGHT: f32 = ACTION_BTN_SIZE;

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
    // Floor for the Result/Error content height, latched by the adapter from
    // the last Processing frame's rendered content (see
    // `OverlayApp::result_latch`) so the final answer never renders shorter
    // than the last streaming frame — the fix for the visible
    // Processing→Result resize jump. `None` outside Result/Error, or when no
    // latch is active (falls back to normal auto-sizing). Growth above this
    // floor (e.g. an expanded Think section) is unrestricted.
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
        .inner_margin(egui::Margin::symmetric(16, 14))
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

                // Never render the Result/Error content shorter than the
                // latched floor from the last Processing frame (see
                // `min_result_height` doc comment) — this is what makes
                // `desired_size` naturally equal-or-taller than the last
                // streaming frame's size, so the final answer's window
                // doesn't visibly shrink relative to it. Growth above the
                // floor (e.g. an expanded Think section) is unaffected.
                if let Some(h) = min_result_height
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
                // The badge tells the user where the content came from; in the
                // Error state the message itself already says what failed, and
                // the last source may be stale (e.g. a startup config notice).
                let source_label =
                    (!matches!(state, OverlayState::Error(_))).then(|| source.label());

                if matches!(state, OverlayState::Capturing) {
                    render_tab_bar(
                        ui, mode, ProcessMode::ALL,
                        thinking, pinned, preview_mode, source_label,
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
                    thinking, pinned, preview_mode, source_label,
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
                            &mut action,
                        );
                    }
                    OverlayState::Error(msg) => {
                        // Retry needs loaded content; a capture failure leaves
                        // none (available_modes is empty), so hide the button.
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
    // the floating action button. Cmd/Ctrl+C copies the full result, but only
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

/// Renders a vertically scrollable, word-wrapped text label with a consistent style.
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
    if can_retry || debug_available {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if can_retry {
                let retry_btn = egui::Button::new(
                    egui::RichText::new("Retry").size(12.0).color(egui::Color32::WHITE),
                )
                .fill(egui::Color32::from_rgba_unmultiplied(50, 50, 50, 200))
                .corner_radius(6.0);
                if ui.add(retry_btn).clicked() {
                    *action = OverlayAction::Retry;
                }
            }
            if debug_available {
                let debug_btn = egui::Button::new(
                    egui::RichText::new("Copy debug")
                        .size(12.0)
                        .color(egui::Color32::from_gray(190)),
                )
                .fill(egui::Color32::from_rgba_unmultiplied(50, 50, 50, 200))
                .corner_radius(6.0);
                if ui
                    .add(debug_btn)
                    .on_hover_text("Copy the raw request + response to the clipboard")
                    .clicked()
                {
                    *action = OverlayAction::CopyDebug;
                }
            }
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
    // `TOP_ROW_HEIGHT`) — reserved even with no think content, so the text
    // block below doesn't shift depending on whether this particular result
    // has a think section or not (Processing always shows *some* status row).
    fixed_height_row(ui, TOP_ROW_HEIGHT, |ui| {
        if think_content.is_some() {
            render_think_toggle_header(ui, think_expanded, action);
        }
    });
    // Expanded think content is deliberate, user-triggered growth — kept
    // outside the fixed slot above (unaffected styling/size — #6).
    if think_expanded && let Some(content) = think_content {
        render_think_content(ui, content);
    }
    ui.add_space(4.0);

    // Action buttons: always rendered at top-right of result area.
    // auto_copy (double-tap): paste/replace button (↩)
    // !auto_copy (single-tap): copy button (📋)
    // plus a retry button (↻) to its left, for a fresh generation.
    // Opacity changes on hover (subtle when idle, prominent when hovered — #24).
    let result_top = ui.cursor().min;
    render_scrollable_text(ui, ("result", mode), text, MAX_RESULT_HEIGHT, false);

    // Bottom row: reserved space matching Processing's Cancel-button row (see
    // `BOTTOM_ROW_HEIGHT`), so the pinned total lines up even though the
    // action buttons below float over the text rather than consuming layout
    // space of their own.
    ui.add_space(4.0);
    fixed_height_row(ui, BOTTOM_ROW_HEIGHT, |_ui| {});

    let btn_size = egui::vec2(ACTION_BTN_SIZE, ACTION_BTN_SIZE);
    let btn_pos = egui::pos2(
        result_top.x + OVERLAY_WIDTH - btn_size.x - 2.0,
        result_top.y + 2.0,
    );
    let btn_rect = egui::Rect::from_min_size(btn_pos, btn_size);

    // ✓ confirms a just-completed copy for a moment (#16a).
    let icon = if auto_copy {
        "\u{21a9}"
    } else if copy_confirmed {
        "\u{2713}"
    } else {
        "\u{1f4cb}"
    };
    if floating_action_button(ui, btn_rect, icon) {
        *action = if auto_copy {
            OverlayAction::PasteReplace
        } else {
            OverlayAction::CopyToClipboard
        };
    }

    let retry_rect = btn_rect.translate(egui::vec2(-(ACTION_BTN_SIZE + 4.0), 0.0));
    if floating_action_button(ui, retry_rect, "\u{21bb}") {
        *action = OverlayAction::Retry;
    }

    // Copy-debug (🔍): copies the raw request + response snapshot. Shown only
    // when a capture exists for this result.
    if debug_available {
        let debug_rect = retry_rect.translate(egui::vec2(-(ACTION_BTN_SIZE + 4.0), 0.0));
        if floating_action_button(ui, debug_rect, "\u{1f50d}") {
            *action = OverlayAction::CopyDebug;
        }
    }
}

/// Floating action button with distance-based fade: fully transparent beyond
/// [`ACTION_BTN_FADE_RADIUS`] from the cursor, ramping to [`ACTION_BTN_ALPHA_MAX`]
/// at the button. Returns true when clicked.
fn floating_action_button(ui: &mut egui::Ui, rect: egui::Rect, icon: &str) -> bool {
    let alpha = ui.input(|i| {
        i.pointer.hover_pos().map_or(0u8, |p| {
            let dist = rect.center().distance(p);
            if dist >= ACTION_BTN_FADE_RADIUS {
                0
            } else {
                ((1.0 - dist / ACTION_BTN_FADE_RADIUS) * ACTION_BTN_ALPHA_MAX) as u8
            }
        })
    });
    let btn = egui::Button::new(
        egui::RichText::new(icon)
            .size(14.0)
            .color(egui::Color32::from_rgba_unmultiplied(255, 255, 255, alpha)),
    )
    .fill(egui::Color32::from_rgba_unmultiplied(50, 50, 50, alpha))
    .stroke(egui::Stroke::NONE)
    .corner_radius(4.0);
    ui.put(rect, btn).clicked()
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
    source_label: Option<&'static str>,
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
        // Mode tabs (left side)
        for &mode in ProcessMode::ALL {
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
            // Explain why a tab is disabled (e.g. image-only clipboard locks all
            // modes except Summarize) instead of silently swallowing the click.
            let response = if !is_available && has_content {
                response.on_hover_text("Requires text — image-only clipboard")
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

            // Source badge (leftmost of the right cluster) — where the content
            // came from: "Selection" (double-tap) vs "Clipboard" (single-tap).
            // Makes a slow double-tap that resolved to a single-tap — sending
            // stale clipboard content — visibly different (#50).
            if let Some(label) = source_label {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(label)
                        .size(11.0)
                        .color(egui::Color32::from_gray(120)),
                );
            }
        });
    });
}
