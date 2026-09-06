//! Shared components (docs/UI-GUIDELINES.md §2–3): every control that
//! appears in more than one view lives here, styled with `theme` tokens only.
//! Widgets return what happened (`bool` / `Option<T>`); mapping that onto an
//! overlay action is the view's job.

use eframe::egui;

use super::theme::{color, font, size, space};

pub(super) fn hint_text(text: &str) -> egui::RichText {
    egui::RichText::new(text).color(color::text_muted()).size(font::MICRO)
}

pub(super) fn row_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(color::text_soft()).size(font::LABEL));
}

/// Small caps-style group title with a rule under it.
pub(super) fn section_header(ui: &mut egui::Ui, text: &str) {
    ui.add_space(space::XS);
    ui.label(
        egui::RichText::new(text.to_ascii_uppercase())
            .color(color::text_muted())
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
        color::text()
    } else {
        color::text_secondary()
    });
    let fill = match (selected, tone) {
        (true, PillTone::Accent) => color::accent_fill(),
        (true, PillTone::Quiet) => color::surface_raised(),
        (false, _) => color::surface_subtle(),
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
    egui::Button::new(egui::RichText::new(text).size(font::CAPTION).color(color::text_secondary()))
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
/// natural height up to `max_height`. Rows continuing past the top or bottom
/// edge fade out (#29) — egui's floating scrollbar only shows on hover, so a
/// full panel would otherwise read as complete. The fade lowers the text's
/// own alpha row by row: painting a gradient over the text would composite
/// a second time over the translucent frame and show as a darker box on any
/// light desktop. ↑/↓ scroll by a line, PageUp/PageDown/Space by a page,
/// Home/End to either end (UI-GUIDELINES §4). Views draw body text through
/// `panel::Body::fill_text`, which supplies the height.
pub(super) fn scroll_text(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    text: &str,
    text_color: egui::Color32,
    max_height: f32,
    stick_to_bottom: bool,
) {
    egui::ScrollArea::vertical()
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
            // The content ui starts at the (scrolled) content top; its clip
            // rect is the viewport plus egui's small clip margin.
            let viewport = ui.clip_rect().shrink(ui.visuals().clip_rect_margin);
            let offset = viewport.top() - ui.cursor().top();
            let font_id = egui::FontId::proportional(font::BODY);
            let wrap_width = ui.available_width();
            let plain = egui::text::LayoutJob::simple(text.to_owned(), font_id.clone(), text_color, wrap_width);
            let galley = ui.fonts_mut(|f| f.layout_job(plain));
            let faded = faded_job(text, &galley, font_id, text_color, wrap_width, offset, viewport.height());
            let galley = ui.fonts_mut(|f| f.layout_job(faded));
            ui.add(egui::Label::new(galley).wrap_mode(egui::TextWrapMode::Wrap));
        });
}

/// `text` laid out like `galley` (same font, wrap width and rows), with the
/// rows inside the top/bottom `size::FADE_HEIGHT` of the viewport dimmed in
/// proportion to how deep into that band they sit — only along an edge that
/// actually hides more content (`fade_edges`). `offset` is the scroll offset,
/// `viewport_height` the visible height.
fn faded_job(
    text: &str,
    galley: &egui::Galley,
    font_id: egui::FontId,
    color: egui::Color32,
    wrap_width: f32,
    offset: f32,
    viewport_height: f32,
) -> egui::text::LayoutJob {
    let (fade_top, fade_bottom) = fade_edges(galley.size().y, viewport_height, offset);
    let mut job = egui::text::LayoutJob {
        text: text.to_owned(),
        wrap: egui::text::TextWrapping { max_width: wrap_width, ..Default::default() },
        ..Default::default()
    };
    let mut chars = text.char_indices();
    let mut byte = 0usize;
    for row in &galley.rows {
        let start = byte;
        for _ in 0..row.char_count_including_newline() {
            if let Some((i, c)) = chars.next() {
                byte = i + c.len_utf8();
            }
        }
        if start == byte {
            continue;
        }
        let mid = (row.min_y() + row.max_y()) / 2.0 - offset;
        let alpha = row_alpha(mid, viewport_height, fade_top, fade_bottom);
        job.sections.push(egui::text::LayoutSection {
            leading_space: 0.0,
            byte_range: start..byte,
            format: egui::TextFormat { font_id: font_id.clone(), color: color.gamma_multiply(alpha), ..Default::default() },
        });
    }
    if byte < text.len() {
        job.sections.push(egui::text::LayoutSection {
            leading_space: 0.0,
            byte_range: byte..text.len(),
            format: egui::TextFormat { font_id, color, ..Default::default() },
        });
    }
    job
}

/// Opacity of a row whose center sits `mid` below the viewport top: 1 in the
/// middle, ramping to 0 at a faded edge.
fn row_alpha(mid: f32, viewport_height: f32, fade_top: bool, fade_bottom: bool) -> f32 {
    let mut alpha: f32 = 1.0;
    if fade_top {
        alpha = alpha.min((mid / size::FADE_HEIGHT).clamp(0.0, 1.0));
    }
    if fade_bottom {
        alpha = alpha.min(((viewport_height - mid) / size::FADE_HEIGHT).clamp(0.0, 1.0));
    }
    alpha
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



/// The clickable "▶/▼ Thinking" toggle (icon + label; size deliberately the
/// same as the tab labels — #6). Rendered in a `size::ROW` status row; the
/// expanded block is `think_content`, drawn separately below it. Returns
/// true when clicked.
pub(super) fn think_toggle(ui: &mut egui::Ui, expanded: bool) -> bool {
    let icon = if expanded { "\u{25bc}" } else { "\u{25b6}" };
    let btn = egui::Button::new(
        egui::RichText::new(format!("{icon} Thinking"))
            .color(color::text_secondary())
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
                        .color(color::text_muted())
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
                .color(color::text_muted())
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
            .color(color::danger()),
    )
    .fill(color::danger_fill())
    .corner_radius(size::RADIUS);
    ui.add(cancel_btn).clicked()
}

/// Docked action button for the bottom controls row: a fixed
/// [`size::ACTION_BTN`] square, always visible in a subdued tone that
/// brightens on hover. Returns true when clicked.
pub(super) fn docked_action_button(ui: &mut egui::Ui, icon: &str, tooltip: &str) -> bool {
    docked_action_button_enabled(ui, icon, tooltip, true)
}

/// A docked action button that can be disabled. The tooltip shows either way:
/// a disabled control says why instead of swallowing the click.
pub(super) fn docked_action_button_enabled(ui: &mut egui::Ui, icon: &str, tooltip: &str, enabled: bool) -> bool {
    let hovered = ui
        .ctx()
        .read_response(ui.next_auto_id())
        .is_some_and(|r| r.hovered());
    let (fg, bg) = if !enabled {
        (color::text_disabled(), color::surface_subtle())
    } else if hovered {
        (color::text(), color::surface_hover())
    } else {
        (color::text_secondary(), color::surface_subtle())
    };
    let btn = egui::Button::new(egui::RichText::new(icon).size(font::LABEL).color(fg))
        .min_size(egui::vec2(size::ACTION_BTN, size::ACTION_BTN))
        .fill(bg)
        .stroke(egui::Stroke::NONE)
        .corner_radius(size::RADIUS_SM);
    ui.add_enabled(enabled, btn).on_hover_text(tooltip).on_disabled_hover_text(tooltip).clicked()
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
                .color(color::text_muted())
                .size(font::CAPTION),
        );
        for &item in all {
            let is_selected = item == current;
            let text = egui::RichText::new(get_label(item))
                .size(font::CAPTION)
                .color(if is_selected {
                    color::text()
                } else {
                    color::text_muted()
                });
            let button = egui::Button::new(text)
                .fill(if is_selected {
                    color::surface_raised()
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
    use super::{fade_edges, keyboard_scroll_step, row_alpha};
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
    fn row_alpha_ramps_only_at_faded_edges() {
        // Nothing hidden: fully opaque everywhere.
        assert_eq!(row_alpha(5.0, 200.0, false, false), 1.0);
        // Bottom hidden: a row at the very bottom edge vanishes, one a band
        // height above it is opaque, one mid-band is half.
        assert_eq!(row_alpha(200.0, 200.0, false, true), 0.0);
        assert_eq!(row_alpha(200.0 - 28.0, 200.0, false, true), 1.0);
        assert!((row_alpha(200.0 - 14.0, 200.0, false, true) - 0.5).abs() < 1e-6);
        // Top hidden: mirrored.
        assert_eq!(row_alpha(0.0, 200.0, true, false), 0.0);
        assert_eq!(row_alpha(28.0, 200.0, true, false), 1.0);
        // The bottom row is unaffected by a top fade.
        assert_eq!(row_alpha(190.0, 200.0, true, false), 1.0);
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
