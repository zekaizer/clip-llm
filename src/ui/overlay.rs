use eframe::egui;

use super::state_machine::OverlayState;
use crate::{ProcessMode, RephraseLength, RephraseParams, RephraseStyle, ThinkingMode};

const OVERLAY_WIDTH: f32 = 480.0;
const MAX_RESULT_HEIGHT: f32 = 260.0;
/// Space around the frame for shadow rendering.
const SHADOW_PAD: f32 = 20.0;

/// Streaming and think-block display state for Processing/Result rendering.
pub struct StreamingState<'a> {
    pub text: &'a str,
    pub think_started: bool,
    pub think_content: Option<&'a str>,
    pub think_expanded: bool,
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
}

pub struct OverlayOutput {
    pub action: OverlayAction,
    /// Desired viewport size based on rendered content.
    pub desired_size: Option<egui::Vec2>,
    /// Raw content size before shadow padding (used by diagnostics).
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    pub content_size: Option<egui::Vec2>,
}

/// Render the overlay panel. Returns action and desired viewport size.
#[allow(clippy::too_many_arguments)]
pub fn render(
    state: &OverlayState,
    mode: ProcessMode,
    streaming: StreamingState<'_>,
    available_modes: &[ProcessMode],
    rephrase_params: RephraseParams,
    thinking: ThinkingState,
    auto_copy: bool,
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

                render_tab_bar(
                    ui, mode, available_modes,
                    thinking,
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
                        render_processing(ui, mode, streaming.text, streaming.think_started, &mut action);
                    }
                    OverlayState::Result(text) => {
                        render_result(
                            ui,
                            mode,
                            text,
                            streaming.think_content,
                            streaming.think_expanded,
                            auto_copy,
                            &mut action,
                        );
                    }
                    OverlayState::Error(msg) => render_error(ui, msg),
                    OverlayState::Hidden => unreachable!(),
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

    // Viewport = content + shadow padding on all sides.
    let content_size = area_resp.response.rect.size();
    let desired = content_size + egui::vec2(SHADOW_PAD * 2.0, SHADOW_PAD * 2.0);

    OverlayOutput {
        action,
        desired_size: Some(desired),
        content_size: Some(content_size),
    }
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

fn render_think_toggle(
    ui: &mut egui::Ui,
    expanded: bool,
    content: &str,
    action: &mut OverlayAction,
) {
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
    if expanded {
        egui::ScrollArea::vertical()
            .id_salt("think_content")
            .max_height(120.0)
            .auto_shrink([false, true])
            .scroll_bar_visibility(
                egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
            )
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
}

fn render_processing(
    ui: &mut egui::Ui,
    mode: ProcessMode,
    streaming_text: &str,
    think_started: bool,
    action: &mut OverlayAction,
) {
    if think_started && streaming_text.is_empty() {
        // Think block in progress, no visible output yet.
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                egui::RichText::new("Thinking...")
                    .color(egui::Color32::from_gray(160))
                    .size(15.0),
            );
        });
    } else if think_started {
        // Think done, answer streaming: show locked collapsed header.
        ui.label(
            egui::RichText::new("\u{25b6} Thinking")
                .color(egui::Color32::from_gray(100))
                .size(13.0),
        );
        ui.add_space(4.0);
        render_scrollable_text(ui, ("streaming", mode), streaming_text, MAX_RESULT_HEIGHT, true);
    } else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                egui::RichText::new(mode.processing_label())
                    .color(egui::Color32::WHITE)
                    .size(15.0),
            );
        });
        if !streaming_text.is_empty() {
            ui.add_space(4.0);
            render_scrollable_text(
                ui,
                ("streaming", mode),
                streaming_text,
                MAX_RESULT_HEIGHT,
                true,
            );
        }
    }
    ui.add_space(4.0);
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

fn render_error(ui: &mut egui::Ui, message: &str) {
    ui.label(
        egui::RichText::new(format!("Error: {message}"))
            .color(egui::Color32::from_rgb(255, 100, 100))
            .size(14.0),
    );
}

fn render_result(
    ui: &mut egui::Ui,
    mode: ProcessMode,
    text: &str,
    think_content: Option<&str>,
    think_expanded: bool,
    auto_copy: bool,
    action: &mut OverlayAction,
) {
    if let Some(content) = think_content {
        render_think_toggle(ui, think_expanded, content, action);
        ui.add_space(4.0);
    }

    // Action button: always rendered at top-right of result area.
    // auto_copy (double-tap): paste/replace button (↩)
    // !auto_copy (single-tap): copy button (📋)
    // Opacity changes on hover (subtle when idle, prominent when hovered).
    let result_top = ui.cursor().min;
    render_scrollable_text(ui, ("result", mode), text, MAX_RESULT_HEIGHT, false);

    let btn_size = egui::vec2(26.0, 26.0);
    let btn_pos = egui::pos2(
        result_top.x + OVERLAY_WIDTH - btn_size.x - 16.0,
        result_top.y + 2.0,
    );
    let btn_rect = egui::Rect::from_min_size(btn_pos, btn_size);

    let hovered = ui.input(|i| {
        i.pointer.hover_pos().is_some_and(|p| btn_rect.contains(p))
    });
    let alpha = if hovered { 200 } else { 30 };
    let icon = if auto_copy { "\u{21a9}" } else { "\u{1f4cb}" };
    let btn = egui::Button::new(
        egui::RichText::new(icon).size(14.0),
    )
    .fill(egui::Color32::from_rgba_unmultiplied(50, 50, 50, alpha))
    .corner_radius(4.0);

    if ui.put(btn_rect, btn).clicked() {
        *action = if auto_copy {
            OverlayAction::PasteReplace
        } else {
            OverlayAction::CopyToClipboard
        };
    }
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
                    egui::Color32::from_gray(100)
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
}

fn render_tab_bar(
    ui: &mut egui::Ui,
    current: ProcessMode,
    available_modes: &[ProcessMode],
    thinking: ThinkingState,
    action: &mut OverlayAction,
) {
    ui.horizontal(|ui| {
        // Mode tabs (left side)
        for &mode in ProcessMode::ALL {
            let is_available = available_modes.contains(&mode);
            let is_selected = mode == current && is_available;

            let text = egui::RichText::new(mode.label())
                .size(13.0)
                .color(if !is_available {
                    egui::Color32::from_gray(50)
                } else if is_selected {
                    egui::Color32::WHITE
                } else {
                    egui::Color32::from_gray(100)
                });

            let button = egui::Button::new(text)
                .fill(if is_selected {
                    egui::Color32::from_rgba_unmultiplied(60, 60, 60, 200)
                } else {
                    egui::Color32::TRANSPARENT
                })
                .corner_radius(6.0);

            if ui.add(button).clicked() && !is_selected && is_available {
                *action = OverlayAction::SwitchMode(mode);
            }
        }

        // Thinking pill (right side) — hidden when model doesn't support thinking control.
        if thinking.supported {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Render in reverse order (right-to-left layout reverses visual order)
                for &tm in ThinkingMode::ALL.iter().rev() {
                    let is_selected = tm == thinking.mode;

                    let text = egui::RichText::new(tm.label())
                        .size(11.0)
                        .color(if is_selected {
                            egui::Color32::WHITE
                        } else {
                            egui::Color32::from_gray(80)
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
            });
        }
    });
}
