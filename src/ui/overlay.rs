use eframe::egui;

use super::OverlayState;
use crate::ProcessMode;

const OVERLAY_WIDTH: f32 = 480.0;
const MAX_RESULT_HEIGHT: f32 = 260.0;
/// Space around the frame for shadow rendering.
const SHADOW_PAD: f32 = 20.0;

/// Action requested by the overlay UI.
pub enum OverlayAction {
    None,
    Close,
    Cancel,
    StartDrag,
    SwitchMode(ProcessMode),
}

pub struct OverlayOutput {
    pub action: OverlayAction,
    /// Desired viewport size based on rendered content.
    pub desired_size: Option<egui::Vec2>,
}

/// Render the overlay panel. Returns action and desired viewport size.
pub fn render(state: &OverlayState, mode: ProcessMode, ctx: &egui::Context) -> OverlayOutput {
    if matches!(state, OverlayState::Hidden) {
        return OverlayOutput {
            action: OverlayAction::None,
            desired_size: None,
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

                render_tab_bar(ui, mode, &mut action);

                // Separator between tab bar and content.
                ui.add_space(4.0);
                ui.add(egui::Separator::default().spacing(4.0));
                ui.add_space(4.0);

                match state {
                    OverlayState::Processing => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new(mode.processing_label())
                                    .color(egui::Color32::WHITE)
                                    .size(15.0),
                            );
                        });
                        ui.add_space(4.0);
                        let cancel_btn = egui::Button::new(
                            egui::RichText::new("Cancel")
                                .size(12.0)
                                .color(egui::Color32::from_rgb(255, 140, 140)),
                        )
                        .fill(egui::Color32::from_rgba_unmultiplied(80, 30, 30, 180))
                        .corner_radius(6.0);
                        if ui.add(cancel_btn).clicked() {
                            action = OverlayAction::Cancel;
                        }
                    }
                    OverlayState::Result(text) => {
                        egui::ScrollArea::vertical()
                            .max_height(MAX_RESULT_HEIGHT)
                            .auto_shrink([false, true])
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                            )
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(text)
                                            .color(egui::Color32::WHITE)
                                            .size(15.0),
                                    )
                                    .wrap_mode(egui::TextWrapMode::Wrap),
                                );
                            });
                    }
                    OverlayState::Error(msg) => {
                        ui.label(
                            egui::RichText::new(format!("Error: {msg}"))
                                .color(egui::Color32::from_rgb(255, 100, 100))
                                .size(14.0),
                        );
                    }
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
    }
}

fn render_tab_bar(ui: &mut egui::Ui, current: ProcessMode, action: &mut OverlayAction) {
    ui.horizontal(|ui| {
        for &mode in ProcessMode::ALL {
            let is_selected = mode == current;
            let text = egui::RichText::new(mode.label())
                .size(13.0)
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
                *action = OverlayAction::SwitchMode(mode);
            }
        }
    });
}
