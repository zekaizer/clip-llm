//! Shared components (docs/UI-GUIDELINES.md §2–3): every control that
//! appears in more than one view lives here, styled with `theme` tokens only.
//! Widgets return what happened (`bool` / `Option<T>`); mapping that onto an
//! overlay action is the view's job.

use eframe::egui;

use super::theme::{color, font, size, space};

pub(super) fn hint_text(text: &str) -> egui::RichText {
    egui::RichText::new(text).color(color::TEXT_MUTED).size(font::MICRO)
}

pub(super) fn row_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(color::TEXT_SOFT).size(font::LABEL));
}

/// Small caps-style group title with a rule under it.
pub(super) fn section_header(ui: &mut egui::Ui, text: &str) {
    ui.add_space(space::XS);
    ui.label(
        egui::RichText::new(text.to_ascii_uppercase())
            .color(color::TEXT_MUTED)
            .size(font::MICRO),
    );
    ui.add(egui::Separator::default().spacing(space::SM));
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PillTone {
    /// Selected = accent fill (an explicit choice).
    Accent,
    /// Selected = neutral fill (the built-in default is in effect).
    Quiet,
}

/// Selectable pill in the tab-bar/param-pill style. Returns true on click.
pub(super) fn pill(ui: &mut egui::Ui, text: &str, selected: bool) -> bool {
    pill_styled(ui, text, selected, PillTone::Accent).clicked()
}

pub(super) fn pill_with_tip(ui: &mut egui::Ui, text: &str, selected: bool, tip: &str) -> bool {
    pill_styled(ui, text, selected, PillTone::Accent).on_hover_text(tip).clicked()
}

pub(super) fn pill_styled(ui: &mut egui::Ui, text: &str, selected: bool, tone: PillTone) -> egui::Response {
    let rich = egui::RichText::new(text).size(font::CAPTION).color(if selected {
        color::TEXT
    } else {
        color::TEXT_SECONDARY
    });
    let fill = match (selected, tone) {
        (true, PillTone::Accent) => color::ACCENT_FILL,
        (true, PillTone::Quiet) => color::SURFACE_RAISED,
        (false, _) => color::SURFACE_SUBTLE,
    };
    let button = egui::Button::new(rich)
        .fill(fill)
        .stroke(egui::Stroke::NONE)
        .corner_radius(size::RADIUS)
        .min_size(egui::vec2(0.0, size::ROW));
    ui.add(button)
}

/// Flat, low-emphasis button for secondary actions.
pub(super) fn small_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).size(font::CAPTION).color(color::TEXT_SECONDARY))
        .fill(egui::Color32::TRANSPARENT)
        .stroke(egui::Stroke::NONE)
        .corner_radius(size::RADIUS)
}

/// Dropdown of common languages plus a free-text field for anything else.
pub(super) fn language_picker(ui: &mut egui::Ui, id: &str, value: &mut String) {
    let common = crate::settings::COMMON_LANGUAGES;
    let is_common = common.iter().any(|l| l.eq_ignore_ascii_case(value.trim()));
    let selected = if is_common { value.trim().to_string() } else { "Other\u{2026}".to_string() };
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt(id)
            .width(140.0)
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for lang in common {
                    if ui.selectable_label(value.trim().eq_ignore_ascii_case(lang), *lang).clicked() {
                        *value = (*lang).to_string();
                    }
                }
                if ui.selectable_label(!is_common, "Other\u{2026}").clicked() && is_common {
                    value.clear();
                }
            });
        if !is_common {
            ui.add(
                egui::TextEdit::singleline(value)
                    .desired_width(150.0)
                    .hint_text("language name"),
            );
        }
    });
}

/// Render `add_contents` inside a row whose height is pinned to exactly
/// `height` (a floor — content is never clipped, only ever centered within
/// more space than it naturally needs). Used for the shared top status/think
/// row and bottom controls row in both Processing and Result, so those rows'
/// vertical footprint is identical regardless of which content variant (or
/// which state) renders inside them — only the *contents* swap in place at
/// the Processing→Result transition, not the layout around them.
pub(super) fn fixed_height_row(ui: &mut egui::Ui, height: f32, add_contents: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(height);
        add_contents(ui);
    });
}

/// Renders a vertically scrollable, word-wrapped text label with a consistent
/// style, shrinking to the content's natural height up to `max_height`.
pub(super) fn scroll_text(
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
        .min_scrolled_height(size::MIN_TEXT_HEIGHT)
        .auto_shrink([false, true])
        .stick_to_bottom(stick_to_bottom)
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(text).color(color::TEXT).size(font::BODY),
                )
                .wrap_mode(egui::TextWrapMode::Wrap),
            );
        });
}

/// The clickable "▶/▼ Thinking" toggle (icon + label; size deliberately the
/// same as the tab labels — #6). Rendered in a `size::ROW` status row; the
/// expanded block is `think_content`, drawn separately below it. Returns
/// true when clicked.
pub(super) fn think_toggle(ui: &mut egui::Ui, expanded: bool) -> bool {
    let icon = if expanded { "\u{25bc}" } else { "\u{25b6}" };
    let btn = egui::Button::new(
        egui::RichText::new(format!("{icon} Thinking"))
            .color(color::TEXT_SECONDARY)
            .size(font::LABEL),
    )
    .fill(egui::Color32::TRANSPARENT);
    ui.add(btn).clicked()
}

/// The expanded think block (`think_block`): muted, scrollable, capped at
/// `size::THINK_MAX_HEIGHT` so it never crowds out the answer.
pub(super) fn think_block(ui: &mut egui::Ui, content: &str) {
    egui::ScrollArea::vertical()
        .id_salt("think_content")
        .max_height(size::THINK_MAX_HEIGHT)
        .auto_shrink([false, true])
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(content)
                        .color(color::TEXT_MUTED)
                        .size(font::LABEL),
                )
                .wrap_mode(egui::TextWrapMode::Wrap),
            );
        });
}

/// Small dim label showing how long the current request has been processing.
/// Helps the user distinguish slow generation (especially a long thinking phase)
/// from a stall.
pub(super) fn elapsed_label(ui: &mut egui::Ui, elapsed: Option<std::time::Duration>) {
    if let Some(d) = elapsed {
        ui.label(
            egui::RichText::new(format!("{:.1}s", d.as_secs_f32()))
                .color(color::TEXT_MUTED)
                .size(font::CAPTION),
        );
    }
}

/// The "Cancel" button every in-flight state offers (Capturing, Processing).
/// Returns true when clicked.
pub(super) fn cancel_button(ui: &mut egui::Ui) -> bool {
    let cancel_btn = egui::Button::new(
        egui::RichText::new("Cancel")
            .size(font::CAPTION)
            .color(color::DANGER),
    )
    .fill(color::DANGER_FILL)
    .corner_radius(size::RADIUS);
    ui.add(cancel_btn).clicked()
}

/// Docked action button for the bottom controls row: a fixed
/// [`size::ACTION_BTN`] square, always visible in a subdued tone that
/// brightens on hover. Returns true when clicked.
pub(super) fn docked_action_button(ui: &mut egui::Ui, icon: &str, tooltip: &str) -> bool {
    let hovered = ui
        .ctx()
        .read_response(ui.next_auto_id())
        .is_some_and(|r| r.hovered());
    let (fg, bg) = if hovered {
        (color::TEXT, color::SURFACE_HOVER)
    } else {
        (color::TEXT_SECONDARY, color::SURFACE_SUBTLE)
    };
    let btn = egui::Button::new(egui::RichText::new(icon).size(font::LABEL).color(fg))
        .min_size(egui::vec2(size::ACTION_BTN, size::ACTION_BTN))
        .fill(bg)
        .stroke(egui::Stroke::NONE)
        .corner_radius(size::RADIUS_SM);
    ui.add(btn).on_hover_text(tooltip).clicked()
}

/// A labelled row of mutually exclusive pills. Returns the item the user
/// clicked, if it differs from `current`.
pub(super) fn pill_row<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    label: &str,
    all: &[T],
    current: T,
    get_label: impl Fn(T) -> &'static str,
) -> Option<T> {
    let mut picked = None;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(label)
                .color(color::TEXT_MUTED)
                .size(font::CAPTION),
        );
        for &item in all {
            let is_selected = item == current;
            let text = egui::RichText::new(get_label(item))
                .size(font::CAPTION)
                .color(if is_selected {
                    color::TEXT
                } else {
                    color::TEXT_MUTED
                });
            let button = egui::Button::new(text)
                .fill(if is_selected {
                    color::SURFACE_RAISED
                } else {
                    egui::Color32::TRANSPARENT
                })
                .corner_radius(size::RADIUS);
            if ui.add(button).clicked() && !is_selected {
                picked = Some(item);
            }
        }
    });
    picked
}
