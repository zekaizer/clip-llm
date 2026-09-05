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

/// A horizontal row at least `height` tall (content is centered, never
/// clipped) — the status rows and the footer keep the same footprint in every
/// state, so only their contents swap at a transition.
pub(super) fn fixed_height_row(ui: &mut egui::Ui, height: f32, add_contents: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(height);
        add_contents(ui);
    });
}

/// Vertically scrollable, word-wrapped body text, shrinking to the content's
/// natural height up to `max_height`. Text continuing past the top or bottom
/// edge is marked with a fade into the frame (#29) — egui's floating
/// scrollbar only shows on hover, so a full panel would otherwise read as
/// complete. ↑/↓ scroll by a line, PageUp/PageDown/Space by a page, Home/End
/// to either end (UI-GUIDELINES §4). Views draw body text through
/// `panel::Body::fill_text`, which supplies the height.
pub(super) fn scroll_text(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    text: &str,
    text_color: egui::Color32,
    max_height: f32,
    stick_to_bottom: bool,
) {
    let out = egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .max_height(max_height)
        // egui's ScrollArea defaults to a 64px `min_scrolled_size` floor once
        // content needs scrolling, silently overriding a smaller max_height
        // (a `max_height` as low as e.g. 24 was still rendering at ~64px).
        // Match the panel's own text-column floor so a tight budget is honored.
        .min_scrolled_height(size::MIN_TEXT_HEIGHT)
        .auto_shrink([false, true])
        .stick_to_bottom(stick_to_bottom)
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
        .show(ui, |ui| {
            // No focused widget means the keys are ours: the panel has no
            // text fields, and egui only scrolls areas that own the focus.
            if ui.memory(|m| m.focused().is_none())
                && let Some(step) = ui.input_mut(|i| keyboard_scroll_step(i, max_height))
            {
                ui.scroll_with_delta(egui::vec2(0.0, step));
            }
            ui.add(
                egui::Label::new(
                    egui::RichText::new(text).color(text_color).size(font::BODY),
                )
                .wrap_mode(egui::TextWrapMode::Wrap),
            );
        });
    let (top, bottom) = fade_edges(out.content_size.y, out.inner_rect.height(), out.state.offset.y);
    if top {
        paint_fade(ui, out.inner_rect, true);
    }
    if bottom {
        paint_fade(ui, out.inner_rect, false);
    }
}

/// Vertical scroll delta (egui sign: positive moves the content down, i.e.
/// scrolls up) for the navigation key pressed this frame, consuming it.
/// `page` is the visible height.
fn keyboard_scroll_step(input: &mut egui::InputState, page: f32) -> Option<f32> {
    const LINE: f32 = 40.0;
    const FAR: f32 = 1.0e6;
    let none = egui::Modifiers::NONE;
    let steps: [(egui::Key, egui::Modifiers, f32); 7] = [
        (egui::Key::ArrowDown, none, -LINE),
        (egui::Key::ArrowUp, none, LINE),
        (egui::Key::PageDown, none, -page),
        (egui::Key::PageUp, none, page),
        (egui::Key::Space, none, -page),
        (egui::Key::End, none, -FAR),
        (egui::Key::Home, none, FAR),
    ];
    steps.iter().find(|(key, mods, _)| input.consume_key(*mods, *key)).map(|(_, _, step)| *step)
}

/// Which scroll edges hide more content: (`top`, `bottom`). A one-pixel
/// tolerance absorbs layout rounding so a column that just fits stays clean.
pub(super) fn fade_edges(content_height: f32, viewport_height: f32, offset_y: f32) -> (bool, bool) {
    let hidden_below = content_height - viewport_height - offset_y;
    (offset_y > 1.0, hidden_below > 1.0)
}

/// A `size::FADE_HEIGHT` gradient from transparent into the frame color
/// along the `top` or bottom edge of `viewport`, painted over the text.
fn paint_fade(ui: &egui::Ui, viewport: egui::Rect, top: bool) {
    let h = size::FADE_HEIGHT.min(viewport.height());
    let band = if top {
        egui::Rect::from_min_size(viewport.min, egui::vec2(viewport.width(), h))
    } else {
        egui::Rect::from_min_size(
            egui::pos2(viewport.min.x, viewport.max.y - h),
            egui::vec2(viewport.width(), h),
        )
    };
    let (edge, inner) = if top { (band.min.y, band.max.y) } else { (band.max.y, band.min.y) };
    let mut mesh = egui::Mesh::default();
    let opaque = color::SURFACE;
    let clear = egui::Color32::TRANSPARENT;
    mesh.colored_vertex(egui::pos2(band.min.x, edge), opaque);
    mesh.colored_vertex(egui::pos2(band.max.x, edge), opaque);
    mesh.colored_vertex(egui::pos2(band.max.x, inner), clear);
    mesh.colored_vertex(egui::pos2(band.min.x, inner), clear);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    ui.painter().add(egui::Shape::mesh(mesh));
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

/// A `size::ROW` status row at the top of the body: optional spinner, a
/// label (already styled), and the elapsed time.
pub(super) fn status_row(
    ui: &mut egui::Ui,
    spinner: bool,
    label: egui::RichText,
    elapsed: Option<std::time::Duration>,
) {
    fixed_height_row(ui, size::ROW, |ui| {
        if spinner {
            ui.spinner();
        }
        ui.label(label);
        elapsed_label(ui, elapsed);
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

#[cfg(test)]
mod tests {
    use super::{fade_edges, keyboard_scroll_step};
    use eframe::egui;

    fn input_with(key: egui::Key) -> egui::InputState {
        let raw = egui::RawInput {
            events: vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        egui::InputState::default().begin_pass(raw, false, 1.0, egui::InputOptions::default())
    }

    #[test]
    fn navigation_keys_scroll_by_line_page_or_to_an_end() {
        let mut input = input_with(egui::Key::ArrowDown);
        assert_eq!(keyboard_scroll_step(&mut input, 200.0), Some(-40.0));
        assert_eq!(keyboard_scroll_step(&mut input, 200.0), None, "consumed");
        assert_eq!(keyboard_scroll_step(&mut input_with(egui::Key::PageUp), 200.0), Some(200.0));
        assert_eq!(keyboard_scroll_step(&mut input_with(egui::Key::Space), 200.0), Some(-200.0));
        assert!(keyboard_scroll_step(&mut input_with(egui::Key::End), 200.0).unwrap() < -1000.0);
        assert_eq!(keyboard_scroll_step(&mut input_with(egui::Key::Tab), 200.0), None);
    }

    #[test]
    fn fade_marks_only_the_edges_that_hide_content() {
        // Fits: nothing to mark.
        assert_eq!(fade_edges(100.0, 200.0, 0.0), (false, false));
        // Layout rounding within a pixel is not an overflow.
        assert_eq!(fade_edges(200.5, 200.0, 0.0), (false, false));
        // Overflows, scrolled to the top: more below only.
        assert_eq!(fade_edges(500.0, 200.0, 0.0), (false, true));
        // Mid-way: both.
        assert_eq!(fade_edges(500.0, 200.0, 100.0), (true, true));
        // Scrolled to the bottom: more above only.
        assert_eq!(fade_edges(500.0, 200.0, 300.0), (true, false));
    }
}
