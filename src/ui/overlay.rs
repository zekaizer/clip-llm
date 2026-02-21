use eframe::egui;

use super::OverlayState;
use crate::ProcessMode;

const OVERLAY_WIDTH: f32 = 480.0;
const MAX_RESULT_HEIGHT: f32 = 400.0;

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

    let margin = egui::Margin::symmetric(20, 16);
    let frame = egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(30, 30, 30, 230))
        .corner_radius(12)
        .inner_margin(margin)
        .shadow(egui::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: egui::Color32::from_black_alpha(100),
        });

    let area_resp = egui::Area::new("overlay".into())
        .fixed_pos(egui::pos2(0.0, 0.0))
        .sense(egui::Sense::drag())
        .show(ctx, |ui| {
            frame.show(ui, |ui| {
                ui.set_width(OVERLAY_WIDTH);

                render_tab_bar(ui, mode, &mut action);
                ui.add_space(8.0);

                match state {
                    OverlayState::Processing => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new(mode.processing_label())
                                    .color(egui::Color32::WHITE)
                                    .size(18.0),
                            );
                        });
                        ui.add_space(4.0);
                        if ui.small_button("Cancel").clicked() {
                            action = OverlayAction::Cancel;
                        }
                    }
                    OverlayState::Result(text) => {
                        egui::ScrollArea::vertical()
                            .max_height(MAX_RESULT_HEIGHT)
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(text)
                                        .color(egui::Color32::WHITE)
                                        .size(18.0),
                                );
                            });
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("Copied to clipboard")
                                .color(egui::Color32::from_gray(120))
                                .size(13.0),
                        );
                    }
                    OverlayState::Error(msg) => {
                        ui.label(
                            egui::RichText::new(format!("Error: {msg}"))
                                .color(egui::Color32::from_rgb(255, 100, 100))
                                .size(16.0),
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

    // Calculate desired viewport size from the rendered area + padding for shadow.
    let content_size = area_resp.response.rect.size();
    let padding = egui::vec2(40.0, 40.0); // extra space for shadow
    let desired = content_size + padding;

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
                .size(14.0)
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
