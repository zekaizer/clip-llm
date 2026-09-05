//! The overlay panel: one fixed-size frame with header / status / body /
//! footer slots that every view fills the same way (docs/UI-GUIDELINES.md
//! §1–2). Views never size the panel; the size comes from the adapter
//! (config + grip).

use eframe::egui;

use super::theme::{color, size, space};
use super::widgets::{fixed_height_row, scroll_text};

/// What the resize grip reported this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GripAction {
    None,
    /// Being dragged: the new panel size (already clamped to `size::MIN_PANEL`).
    Resize(egui::Vec2),
    /// The drag ended — the size is final.
    Done,
    /// Double-clicked — back to the default size.
    Reset,
}

/// Geometry of the rendered panel.
pub struct PanelOutput {
    /// Frame size incl. margin (equals the requested size unless content
    /// could not fit the minimum).
    pub content_size: egui::Vec2,
    /// Window size: the frame plus the shadow pad on every side.
    pub desired_size: egui::Vec2,
    /// The user started dragging the panel background (move the window).
    pub drag_started: bool,
    pub grip: GripAction,
}

/// The translucent rounded frame every overlay view and the settings panel
/// are drawn in.
pub fn frame() -> egui::Frame {
    egui::Frame::new()
        .fill(color::SURFACE)
        .stroke(egui::Stroke::NONE)
        .corner_radius(size::FRAME_RADIUS)
        .inner_margin(size::FRAME_MARGIN)
        .shadow(egui::Shadow { offset: [0, 4], blur: 16, spread: 0, color: color::SHADOW })
}

/// Draw a panel of exactly `panel_size` (frame incl. margin) and let `view`
/// fill its slots. The frame is floored to that size so a short view still
/// occupies it; a view that overflows (it should not — see `Body`) grows it.
pub fn show(
    ctx: &egui::Context,
    panel_size: egui::Vec2,
    view: impl FnOnce(&mut Slots<'_>),
) -> PanelOutput {
    let panel_size = panel_size.max(size::MIN_PANEL);
    let inner = panel_size - size::FRAME_MARGIN.sum();
    let mut grip = GripAction::None;

    // egui::Area remembers its previous size and, with `constrain` on, caps
    // the first sizing pass at the (small, initial) window — so a fixed-size
    // panel must opt out.
    let area = egui::Area::new("overlay".into())
        .fixed_pos(egui::pos2(size::SHADOW_PAD, size::SHADOW_PAD))
        .constrain(false)
        .sense(egui::Sense::drag())
        .show(ctx, |ui| {
            let frame_resp = frame().show(ui, |ui| {
                ui.set_width(inner.x);
                let top = ui.cursor().top();
                // The floor for the whole panel. Must be set before anything
                // is drawn: `set_min_height` reserves from the current cursor.
                ui.set_min_height(inner.y);
                let mut slots = Slots { ui, bottom: top + inner.y };
                view(&mut slots);
            });
            grip = resize_grip(ui, frame_resp.response.rect);
        });

    let content_size = area.response.rect.size();
    PanelOutput {
        content_size,
        desired_size: content_size + egui::Vec2::splat(size::SHADOW_PAD * 2.0),
        // The grip is a child widget, so a drag it captured never starts here.
        drag_started: area.response.drag_started() && matches!(grip, GripAction::None),
        grip,
    }
}

/// The slots of a view, to be filled top to bottom: `header`, `status`,
/// `body`, `footer`. Every state fills all four, so a transition only swaps
/// contents — nothing below a slot moves.
pub struct Slots<'u> {
    ui: &'u mut egui::Ui,
    /// Bottom edge of the inner content area (frame margin excluded).
    bottom: f32,
}

impl Slots<'_> {
    /// Mode tabs and controls; a separator closes it.
    pub fn header(&mut self, f: impl FnOnce(&mut egui::Ui)) {
        f(self.ui);
        self.ui.add_space(space::SM);
        self.ui.add(egui::Separator::default().spacing(space::SM));
        self.ui.add_space(space::SM);
    }

    /// One `size::ROW` line naming what is going on: spinner and phase while
    /// working, the completion summary and Think toggle afterwards, the
    /// failure otherwise. Always present, so the body never shifts.
    pub fn status(&mut self, f: impl FnOnce(&mut egui::Ui)) {
        fixed_height_row(self.ui, size::ROW, f);
        self.ui.add_space(space::SM);
    }

    /// The state's content. Fills everything between the status row and the
    /// footer; `Body::fill_text` scrolls text within what is left.
    pub fn body(&mut self, f: impl FnOnce(&mut Body<'_>)) {
        let bottom = self.bottom - size::ACTION_BTN - space::SM - self.ui.spacing().item_spacing.y;
        let height = (bottom - self.ui.cursor().top()).max(size::MIN_TEXT_HEIGHT);
        self.ui.scope(|ui| {
            // Floor, so the footer lands at the bottom even for a short body.
            ui.set_min_height(height);
            f(&mut Body { ui, bottom });
        });
        self.ui.add_space(space::SM);
    }

    /// Status on the left, actions on the right (`size::ACTION_BTN` tall).
    pub fn footer(&mut self, f: impl FnOnce(&mut egui::Ui)) {
        fixed_height_row(self.ui, size::ACTION_BTN, f);
    }
}

/// The body slot while a view draws into it.
pub struct Body<'u> {
    pub ui: &'u mut egui::Ui,
    bottom: f32,
}

impl Body<'_> {
    /// Height still available below the cursor.
    pub fn remaining(&self) -> f32 {
        (self.bottom - self.ui.cursor().top()).max(size::MIN_TEXT_HEIGHT)
    }

    /// Wrapped text that fills the remaining body height and scrolls beyond
    /// it — the only way body text is drawn, so nothing outgrows the panel.
    pub fn fill_text(
        &mut self,
        id_salt: impl std::hash::Hash,
        text: &str,
        text_color: egui::Color32,
        stick_to_bottom: bool,
    ) {
        let height = self.remaining();
        self.ui.scope(|ui| {
            ui.set_min_height(height);
            scroll_text(ui, id_salt, text, text_color, height, stick_to_bottom);
        });
    }
}

/// Resize grip in the frame's bottom-right corner (inside the margin, where
/// no content is drawn).
fn resize_grip(ui: &mut egui::Ui, frame_rect: egui::Rect) -> GripAction {
    let grip_rect =
        egui::Rect::from_min_max(frame_rect.max - egui::Vec2::splat(size::GRIP), frame_rect.max);
    let resp = ui
        .interact(grip_rect, ui.id().with("resize_grip"), egui::Sense::click_and_drag())
        .on_hover_cursor(egui::CursorIcon::ResizeSouthEast)
        .on_hover_text("Drag to resize \u{b7} double-click for the default size");
    let tone = if resp.hovered() || resp.dragged() {
        color::TEXT_SECONDARY
    } else {
        color::TEXT_DISABLED
    };
    // Three dots along the corner diagonal, the usual grip glyph.
    let painter = ui.painter();
    let corner = frame_rect.max - egui::vec2(5.0, 5.0);
    for step in 0..3 {
        let offset = step as f32 * 4.0;
        painter.circle_filled(corner - egui::vec2(offset, 0.0), 1.2, tone);
        painter.circle_filled(corner - egui::vec2(0.0, offset), 1.2, tone);
    }
    painter.circle_filled(corner - egui::vec2(4.0, 4.0), 1.2, tone);

    if resp.double_clicked() {
        return GripAction::Reset;
    }
    if resp.drag_stopped() {
        return GripAction::Done;
    }
    let delta = resp.drag_delta();
    if !resp.dragged() || delta == egui::Vec2::ZERO {
        return GripAction::None;
    }
    GripAction::Resize((frame_rect.size() + delta).max(size::MIN_PANEL))
}
