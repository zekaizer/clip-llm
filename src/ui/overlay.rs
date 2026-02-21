use eframe::egui;

use super::OverlayState;

const OVERLAY_WIDTH: f32 = 360.0;

/// Action requested by the overlay UI.
pub enum OverlayAction {
    None,
    Close,
    Cancel,
}

/// Render the overlay panel. Returns the action requested by user interaction.
pub fn render(state: &OverlayState, ctx: &egui::Context) -> OverlayAction {
    if matches!(state, OverlayState::Hidden) {
        return OverlayAction::None;
    }

    let mut action = OverlayAction::None;

    let frame = egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(30, 30, 30, 230))
        .corner_radius(12)
        .inner_margin(egui::Margin::symmetric(16, 12))
        .shadow(egui::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: egui::Color32::from_black_alpha(100),
        });

    egui::Area::new("overlay".into())
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            frame.show(ui, |ui| {
                ui.set_width(OVERLAY_WIDTH);

                match state {
                    OverlayState::Processing => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("Translating...")
                                    .color(egui::Color32::WHITE)
                                    .size(14.0),
                            );
                        });
                        ui.add_space(4.0);
                        if ui
                            .small_button("Cancel")
                            .clicked()
                        {
                            action = OverlayAction::Cancel;
                        }
                    }
                    OverlayState::Result(text) => {
                        egui::ScrollArea::vertical()
                            .max_height(300.0)
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(text)
                                        .color(egui::Color32::WHITE)
                                        .size(14.0),
                                );
                            });
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("Copied to clipboard")
                                .color(egui::Color32::from_gray(120))
                                .size(11.0),
                        );
                    }
                    OverlayState::Error(msg) => {
                        ui.label(
                            egui::RichText::new(format!("Error: {msg}"))
                                .color(egui::Color32::from_rgb(255, 100, 100))
                                .size(13.0),
                        );
                    }
                    OverlayState::Hidden => unreachable!(),
                }
            });
        });

    // Close on Escape key.
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        action = OverlayAction::Close;
    }

    action
}
