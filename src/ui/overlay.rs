use eframe::egui;

use super::state_machine::{CaptureSource, OverlayState};
use super::theme::{color, font, size, space};
use super::widgets::{
    cancel_button, docked_action_button, elapsed_label, fixed_height_row, hint_text, language_picker,
    pill, pill_row, pill_styled, pill_with_tip, row_label, scroll_text, section_header, small_button,
    think_block, think_toggle, PillTone,
};
use crate::{ProcessMode, RephraseLength, RephraseParams, RephraseStyle, ThinkingMode};

pub(crate) const OVERLAY_WIDTH: f32 = 480.0;
const MAX_RESULT_HEIGHT: f32 = 260.0;

/// Inner-content height the panel must fill (see `render()`): a floor for the
/// text column — shorter content pads up to the remainder so the bottom row
/// stays at the bottom — and, with `cap`, also its ceiling, so the text
/// scrolls inside the user's size instead of growing the window.
#[derive(Clone, Copy)]
struct HeightTarget {
    /// Target inner height (frame margin already subtracted).
    inner: f32,
    /// Top of the whole inner content ui, captured before anything is drawn:
    /// the reference for how much of `inner` is already used when the text
    /// column is about to render.
    content_top: f32,
    /// User-sized panel: cap the text column at the remainder too.
    cap: bool,
}

impl HeightTarget {
    /// Height left for the text column: the target minus what is drawn above
    /// the current cursor and what must still follow (`reserved_after`, plus
    /// the item spacing egui inserts after the column itself).
    fn remaining(&self, ui: &egui::Ui, reserved_after: f32) -> f32 {
        let used = ui.cursor().top() - self.content_top;
        (self.inner - used - reserved_after - ui.spacing().item_spacing.y)
            .max(size::MIN_TEXT_HEIGHT)
    }
}

/// Streaming and think-block display state for Processing/Result rendering.
pub struct StreamingState<'a> {
    pub text: &'a str,
    pub think_started: bool,
    /// Status text while an automatic retry is pending (Processing only).
    pub retry_notice: Option<&'a str>,
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
    /// Switch to the next model profile and re-run the current content.
    CycleModel,
    /// The resize grip was dragged; the new panel (frame) size, already
    /// clamped to `size::MIN_PANEL`.
    Resize(egui::Vec2),
    /// The grip drag ended (the size is final — persist it).
    ResizeDone,
    /// The grip was double-clicked: back to auto-sizing.
    ResetSize,
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
    // File names when the content came from a file-list clipboard (badge only).
    source_files: &[String],
    copy_confirmed: bool,
    elapsed: Option<std::time::Duration>,
    debug_available: bool,
    // Compact completion summary ("✓ 2.4s · 850 tokens") shown in Result's
    // bottom row — the same slot Processing's spinner+elapsed+Cancel row
    // occupies (see `size::ROW`/`size::ACTION_BTN`), filling what would
    // otherwise be empty space left by those controls disappearing. `None`
    // when no completion data is available (e.g. a cached/instant result —
    // see `format_completion_status` in `mod.rs`).
    completion_status: Option<String>,
    // More than one model profile exists: the status label switches models.
    model_switchable: bool,
    // Floor for the Result/Error content height, latched by the adapter from
    // the last Processing frame's rendered content (see
    // `OverlayApp::result_latch`) so the final answer never renders shorter
    // than the last streaming frame — the fix for the visible
    // Processing→Result resize jump. `None` outside Result/Error, or when no
    // latch is active (falls back to normal auto-sizing). A floor only:
    // content taller than the latch grows the window naturally, up to
    // `MAX_RESULT_HEIGHT` for the answer text (see `render_result`).
    min_result_height: Option<f32>,
    // Panel (frame) size the user dragged the grip to: replaces both
    // `OVERLAY_WIDTH` and content-driven height — the text column scrolls
    // inside it (`HeightTarget::cap`) and the latch is moot. `None` while
    // the overlay auto-sizes to its content.
    user_size: Option<egui::Vec2>,
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

    let frame = overlay_frame();

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
        .fixed_pos(egui::pos2(size::SHADOW_PAD, size::SHADOW_PAD))
        .constrain(false) // Fix (a): see above
        .sense(egui::Sense::drag())
        .show(ctx, |ui| {
            let frame_resp = frame.show(ui, |ui| {
                // Both the user size and the latch are the Frame's OUTER size
                // (`content_size` in `OverlayOutput`, margin included), so
                // subtract the margin to get targets for the *inner* ui.
                let user_inner = user_size.map(|s| s.max(size::MIN_PANEL) - size::FRAME_MARGIN.sum());
                ui.set_width(user_inner.map_or(OVERLAY_WIDTH, |v| v.x));
                let content_top = ui.cursor().top();

                // User size wins over the latch: it applies in every state and
                // caps the text column. The latch only floors Result/Error
                // (never shorter than the last streaming frame; taller content
                // still grows the window — see `render_result`).
                let target = match user_inner {
                    Some(v) => Some(HeightTarget { inner: v.y, content_top, cap: true }),
                    None => min_result_height
                        .filter(|_| matches!(state, OverlayState::Result(_) | OverlayState::Error(_)))
                        .map(|h| HeightTarget {
                            inner: (h - size::FRAME_MARGIN.sum().y).max(0.0),
                            content_top,
                            cap: false,
                        }),
                };

                // Floor for the whole panel. This MUST run here, at the true
                // top of the inner ui — `Ui::set_min_height` reserves space
                // from the *current cursor*, so applied after content is drawn
                // it would add phantom height on top instead of flooring the
                // total. This is what makes `desired_size` >= the target.
                if let Some(t) = target {
                    ui.set_min_height(t.inner);
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
                    ui.add_space(space::SM);
                    ui.add(egui::Separator::default().spacing(space::SM));
                    ui.add_space(space::SM);
                    render_capturing(ui, picking_text, source, elapsed, target, &mut action);
                    return;
                }

                render_tab_bar(
                    ui, mode, available_modes,
                    thinking, pinned, preview_mode,
                    &mut action,
                );

                // Rephrase parameter rows (style + length), shown when Rephrase is active.
                if mode == ProcessMode::Rephrase && !matches!(state, OverlayState::Hidden) {
                    ui.add_space(space::SM);
                    render_rephrase_params(ui, rephrase_params, &mut action);
                }

                // Separator between tab bar / params and content.
                ui.add_space(space::SM);
                ui.add(egui::Separator::default().spacing(space::SM));
                ui.add_space(space::SM);

                match state {
                    OverlayState::Processing => {
                        render_processing(
                            ui,
                            mode,
                            streaming.text,
                            streaming.think_started,
                            streaming.retry_notice,
                            elapsed,
                            target,
                            &mut action,
                        );
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
                            source_files,
                            completion_status.as_deref(),
                            model_switchable,
                            target,
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
            let grip_action = render_resize_grip(ui, frame_resp.response.rect);
            if !matches!(grip_action, OverlayAction::None) {
                action = grip_action;
            }
        });

    // Drag the OS window when the user drags the overlay area. The grip is a
    // child widget, so a drag it captured never starts here as well.
    if area_resp.response.drag_started() && !matches!(action, OverlayAction::Resize(_)) {
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
    let desired = content_size + egui::vec2(size::SHADOW_PAD * 2.0, size::SHADOW_PAD * 2.0);

    OverlayOutput {
        action,
        desired_size: Some(desired),
        content_size: Some(content_size),
    }
}

/// Resize grip in the frame's bottom-right corner (inside the margin, where
/// no content is drawn): drag = `Resize` (new panel size) then `ResizeDone`
/// on release, double-click = `ResetSize`.
fn render_resize_grip(ui: &mut egui::Ui, frame_rect: egui::Rect) -> OverlayAction {
    let grip_rect = egui::Rect::from_min_max(
        frame_rect.max - egui::vec2(size::GRIP, size::GRIP),
        frame_rect.max,
    );
    let resp = ui
        .interact(grip_rect, ui.id().with("resize_grip"), egui::Sense::click_and_drag())
        .on_hover_cursor(egui::CursorIcon::ResizeSouthEast)
        .on_hover_text("Drag to resize \u{b7} double-click to auto-size");
    let color = if resp.hovered() || resp.dragged() {
        color::TEXT_SECONDARY
    } else {
        color::TEXT_DISABLED
    };
    // Three dots along the corner diagonal, the usual grip glyph.
    let painter = ui.painter();
    let corner = frame_rect.max - egui::vec2(5.0, 5.0);
    for step in 0..3 {
        let offset = step as f32 * 4.0;
        painter.circle_filled(corner - egui::vec2(offset, 0.0), 1.2, color);
        painter.circle_filled(corner - egui::vec2(0.0, offset), 1.2, color);
    }
    painter.circle_filled(corner - egui::vec2(4.0, 4.0), 1.2, color);
    if resp.double_clicked() {
        return OverlayAction::ResetSize;
    }
    if resp.drag_stopped() {
        return OverlayAction::ResizeDone;
    }
    let delta = resp.drag_delta();
    if !resp.dragged() || delta == egui::Vec2::ZERO {
        return OverlayAction::None;
    }
    OverlayAction::Resize((frame_rect.size() + delta).max(size::MIN_PANEL))
}

/// The translucent rounded panel every overlay view is drawn in.
fn overlay_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(color::SURFACE)
        .stroke(egui::Stroke::NONE)
        .corner_radius(size::FRAME_RADIUS)
        .inner_margin(size::FRAME_MARGIN)
        .shadow(egui::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: color::SHADOW,
        })
}

/// What the user did in the settings panel this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsAction {
    None,
    Save,
    Cancel,
    OpenConfig,
    StartDrag,
    /// Run a live connection test for the profile at this index.
    TestProfile(usize),
}

/// Connection-test state shown inside a profile editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileTestView<'a> {
    Idle,
    Running,
    Done(&'a Result<String, String>),
}

/// Render the settings panel (tray "Settings…") in the overlay window. Edits
/// go straight into `form`; `baseline` is the form as opened (or last saved)
/// and drives the dirty state. The caller acts on the returned action.
pub fn render_settings<'t>(
    ctx: &egui::Context,
    form: &mut crate::settings::SettingsForm,
    baseline: Option<&crate::settings::SettingsForm>,
    config_path: Option<&str>,
    test: impl Fn(usize) -> ProfileTestView<'t>,
    caps: impl Fn(&str) -> Option<String>,
) -> (SettingsAction, OverlayOutput) {
    use crate::settings::SettingsForm;
    let mut action = SettingsAction::None;
    let dirty = baseline.is_none_or(|b: &SettingsForm| form.to_patch().ok() != b.to_patch().ok());
    // A fresh edit supersedes the last save's banner.
    if dirty && form.error.is_none() {
        form.notice = None;
    }

    let area_resp = egui::Area::new("settings".into())
        .fixed_pos(egui::pos2(size::SHADOW_PAD, size::SHADOW_PAD))
        .constrain(false)
        .sense(egui::Sense::drag())
        .show(ctx, |ui| {
            overlay_frame().show(ui, |ui| {
                ui.set_width(size::SETTINGS_WIDTH);
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 4.0);
                ui.spacing_mut().slider_width = 220.0;

                // Header: title, file name (full path on hover), close.
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Settings").color(color::TEXT).size(font::TITLE).strong());
                    if let Some(path) = config_path {
                        let name = std::path::Path::new(path)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.to_string());
                        ui.add_space(space::SM);
                        ui.label(hint_text(&name)).on_hover_text(path);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if docked_action_button(ui, "\u{2715}", "Close (Esc)") {
                            action = SettingsAction::Cancel;
                        }
                    });
                });
                ui.add_space(space::XS);

                // A profile being edited gets the panel to itself (a sub-page)
                // so the window never outgrows a laptop screen.
                match form.editing {
                    Some(index) if index < form.profiles.len() => {
                        render_profile_page(ui, form, index, &test, &mut action, dirty);
                    }
                    _ => {
                        form.editing = None;
                        render_settings_body(ui, form, &caps, &mut action, dirty);
                    }
                }
            });
        });


    if area_resp.response.drag_started() {
        action = SettingsAction::StartDrag;
    }
    // Escape closes an open dropdown first, the panel second.
    let popup_open = egui::Popup::is_any_open(ctx);
    if !popup_open && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        action = SettingsAction::Cancel;
    }
    if dirty && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
        action = SettingsAction::Save;
    }

    let content_size = area_resp.response.rect.size();
    let desired = content_size + egui::vec2(size::SHADOW_PAD * 2.0, size::SHADOW_PAD * 2.0);
    (
        action,
        OverlayOutput { action: OverlayAction::None, desired_size: Some(desired), content_size: Some(content_size) },
    )
}

/// The settings sections and footer (everything under the title row).
fn render_settings_body(
    ui: &mut egui::Ui,
    form: &mut crate::settings::SettingsForm,
    caps: &impl Fn(&str) -> Option<String>,
    action: &mut SettingsAction,
    dirty: bool,
) {
    section_header(ui, "Models");
    ui.label(hint_text(
        "\u{25cf} marks the profile used at startup. Switch at runtime from the tray Model menu or by clicking the model name under a result.",
    ));
    render_profiles(ui, form, caps);
    ui.add_space(space::MD);

        section_header(ui, "Languages");
        egui::Grid::new("settings_languages")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                row_label(ui, "Primary");
                language_picker(ui, "primary_lang", &mut form.primary);
                ui.end_row();
                row_label(ui, "Secondary");
                ui.horizontal(|ui| {
                    language_picker(ui, "secondary_lang", &mut form.secondary);
                    if ui
                        .add(small_button("\u{21c4} swap"))
                        .on_hover_text("Swap primary and secondary")
                        .clicked()
                    {
                        std::mem::swap(&mut form.primary, &mut form.secondary);
                    }
                });
                ui.end_row();
            });
        ui.add_space(space::MD);

        section_header(ui, "Behavior");
        egui::Grid::new("settings_behavior")
            .num_columns(2)
            .spacing([16.0, 6.0])
            .show(ui, |ui| {
                row_label(ui, "Mode at startup *");
                ui.horizontal_wrapped(|ui| {
                    for &mode in ProcessMode::ALL {
                        if pill(ui, mode.label(), form.default_mode == mode) {
                            form.default_mode = mode;
                        }
                    }
                });
                ui.end_row();
                row_label(ui, "Double-tap window *");
                let mut ms: u64 = form.double_tap_ms.trim().parse().unwrap_or(350).clamp(100, 2000);
                if ui
                    .add(egui::Slider::new(&mut ms, 100..=1000).suffix(" ms").step_by(10.0))
                    .on_hover_text("How long a first tap waits for a second one (lower = snappier single tap)")
                    .changed()
                {
                    form.double_tap_ms = ms.to_string();
                }
                ui.end_row();
                row_label(ui, "Keep result open");
                ui.horizontal(|ui| {
                    let tip = "Pinned results stay open when the overlay loses focus";
                    if pill_with_tip(ui, "after single-tap", form.single_tap_pinned, tip) {
                        form.single_tap_pinned = !form.single_tap_pinned;
                    }
                    if pill_with_tip(ui, "after double-tap", form.double_tap_pinned, tip) {
                        form.double_tap_pinned = !form.double_tap_pinned;
                    }
                });
                ui.end_row();
            });
        ui.label(hint_text("* applies after a restart"));
        ui.add_space(space::MD);

        section_header(ui, "Thinking");
        egui::Grid::new("settings_thinking")
            .num_columns(2)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                for (mode, thinking) in form.thinking.iter_mut() {
                    row_label(ui, mode.label());
                    ui.horizontal(|ui| {
                        // "Default" selected is the quiet state (gray);
                        // an explicit override is what earns the accent.
                        let built_in = mode.default_thinking().label();
                        if pill_styled(ui, "Default", thinking.is_none(), PillTone::Quiet)
                            .on_hover_text(format!("Built-in default for {}: {built_in}", mode.label()))
                            .clicked()
                        {
                            *thinking = None;
                        }
                        for (value, text) in
                            [(ThinkingMode::Think, "Think"), (ThinkingMode::NoThink, "No Think")]
                        {
                            if pill(ui, text, *thinking == Some(value)) {
                                *thinking = Some(value);
                            }
                        }
                    });
                    ui.end_row();
                }
            });
        ui.add_space(space::LG);

        render_settings_footer(ui, form, action, dirty);
}

/// Status line, then Open Config / Cancel-or-Done / Save.
fn render_settings_footer(
    ui: &mut egui::Ui,
    form: &crate::settings::SettingsForm,
    action: &mut SettingsAction,
    dirty: bool,
) {
    // Footer: status line, then actions.
        ui.add_space(space::XS);
        if let Some(err) = &form.error {
            ui.label(egui::RichText::new(err).color(color::DANGER).size(font::CAPTION));
        } else if let Some(notice) = &form.notice {
            ui.label(egui::RichText::new(notice).color(color::SUCCESS).size(font::CAPTION));
        }
        ui.horizontal(|ui| {
            if ui
                .add(small_button("Open Config\u{2026}"))
                .on_hover_text("Prompts, API endpoints and model profiles are edited in the file")
                .clicked()
            {
                *action = SettingsAction::OpenConfig;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let save = egui::Button::new(
                    egui::RichText::new("Save").color(color::TEXT).size(font::LABEL),
                )
                .fill(if dirty { color::ACCENT } else { color::RULE })
                .stroke(egui::Stroke::NONE)
                .corner_radius(size::RADIUS)
                .min_size(egui::vec2(80.0, 28.0));
                if ui.add_enabled(dirty, save).on_hover_text("\u{2318}S / Ctrl+S").clicked() {
                    *action = SettingsAction::Save;
                }
                let close_label = if dirty { "Cancel" } else { "Done" };
                let close = egui::Button::new(
                    egui::RichText::new(close_label).color(color::TEXT_SOFT).size(font::LABEL),
                )
                .fill(color::SURFACE_RAISED)
                .stroke(egui::Stroke::NONE)
                .corner_radius(size::RADIUS)
                .min_size(egui::vec2(80.0, 28.0));
                if ui.add(close).clicked() {
                    *action = SettingsAction::Cancel;
                }
            });
        });
}

/// Profile list: startup radio, name, summary, `[api]` tag, Edit (opens the
/// profile sub-page) and an "Add profile" row.
fn render_profiles(
    ui: &mut egui::Ui,
    form: &mut crate::settings::SettingsForm,
    caps: &impl Fn(&str) -> Option<String>,
) {
    use crate::settings::ProfileForm;
    for i in 0..form.profiles.len() {
        let probed = caps(form.profiles[i].name.trim());
        ui.horizontal(|ui| {
            let startup = form.default_model == i;
            if ui
                .add(egui::RadioButton::new(startup, ""))
                .on_hover_text("Use this profile at startup")
                .clicked()
            {
                form.default_model = i;
            }
            let profile = &form.profiles[i];
            let name = if profile.name.trim().is_empty() {
                "(unnamed)".to_string()
            } else {
                profile.name.trim().to_string()
            };
            ui.label(egui::RichText::new(name).size(font::LABEL).color(color::TEXT_SOFT));
            ui.label(hint_text(&profile.summary()));
            if profile.from_api_section {
                ui.label(hint_text("[api]")).on_hover_text(
                    "Stored in the [api] section; empty fields fall back to CLIP_LLM_* variables",
                );
            }
            if let Some(text) = &probed {
                ui.label(hint_text(text)).on_hover_text("Detected by the startup/switch probe");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(small_button("Edit \u{203a}")).clicked() {
                    form.editing = Some(i);
                }
            });
        });
    }
    if ui.add(small_button("+ Add profile")).clicked() {
        form.profiles.push(ProfileForm::blank());
        form.editing = Some(form.profiles.len() - 1);
    }
}

/// Sub-page editing one profile: breadcrumb, fields, test/remove, and the
/// shared Save/Cancel footer.
fn render_profile_page<'t>(
    ui: &mut egui::Ui,
    form: &mut crate::settings::SettingsForm,
    i: usize,
    test: &impl Fn(usize) -> ProfileTestView<'t>,
    action: &mut SettingsAction,
    dirty: bool,
) {
    use crate::settings::{ProfileForm, Provider};
    let count = form.profiles.len();
    let test_state = test(i);

    ui.horizontal(|ui| {
        if ui.add(small_button("\u{2039} All settings")).clicked() {
            form.editing = None;
        }
        let title = if form.profiles[i].name.trim().is_empty() {
            "New profile".to_string()
        } else {
            form.profiles[i].name.trim().to_string()
        };
        ui.label(egui::RichText::new(title).size(font::LABEL).color(color::TEXT));
        if form.profiles[i].from_api_section {
            ui.label(hint_text("[api] section \u{b7} empty fields fall back to CLIP_LLM_* variables"));
        }
    });
    ui.add(egui::Separator::default().spacing(6.0));

    let show_key = &mut form.show_key;
    let profile: &mut ProfileForm = &mut form.profiles[i];
    egui::Grid::new(("profile_editor", i))
        .num_columns(2)
        .spacing([16.0, 8.0])
        .show(ui, |ui| {
            row_label(ui, "Name");
            ui.add(egui::TextEdit::singleline(&mut profile.name).desired_width(240.0).hint_text("shown in menus"));
            ui.end_row();
            row_label(ui, "Provider");
            ui.horizontal(|ui| {
                for &p in Provider::ALL {
                    if pill(ui, p.label(), profile.provider == p) {
                        profile.provider = p;
                    }
                }
            });
            ui.end_row();
            match profile.provider {
                Provider::OpenAi => {
                    row_label(ui, "Endpoint");
                    ui.add(
                        egui::TextEdit::singleline(&mut profile.endpoint)
                            .desired_width(380.0)
                            .hint_text("http://host:8000/v1"),
                    );
                    ui.end_row();
                    row_label(ui, "Model");
                    ui.add(
                        egui::TextEdit::singleline(&mut profile.model)
                            .desired_width(380.0)
                            .hint_text("model id served by the endpoint"),
                    );
                    ui.end_row();
                    row_label(ui, "API key");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut profile.api_key)
                                .desired_width(320.0)
                                .password(!*show_key)
                                .hint_text("any non-empty value if the server needs none"),
                        );
                        if ui.add(small_button(if *show_key { "hide" } else { "show" })).clicked() {
                            *show_key = !*show_key;
                        }
                    });
                    ui.end_row();
                }
                Provider::GrokOauth => {
                    row_label(ui, "Model");
                    ui.add(
                        egui::TextEdit::singleline(&mut profile.model)
                            .desired_width(380.0)
                            .hint_text("grok-4.3"),
                    );
                    ui.end_row();
                    row_label(ui, "Auth file");
                    ui.add(
                        egui::TextEdit::singleline(&mut profile.auth_file)
                            .desired_width(380.0)
                            .hint_text("default: ~/.grok/auth.json (run `grok` once to sign in)"),
                    );
                    ui.end_row();
                }
            }
            row_label(ui, "Thinking off via");
            ui.horizontal_wrapped(|ui| {
                for (key, label, why) in crate::settings::THINKING_CONTROLS {
                    if pill_styled(ui, label, profile.thinking_control == *key, PillTone::Accent)
                        .on_hover_text(*why)
                        .clicked()
                    {
                        profile.thinking_control = (*key).to_string();
                    }
                }
            });
            ui.end_row();
            if !profile.from_api_section {
                row_label(ui, "Limits");
                ui.horizontal(|ui| {
                    ui.label(hint_text("max_tokens"));
                    ui.add(egui::TextEdit::singleline(&mut profile.max_tokens).desired_width(70.0).hint_text("global"));
                    ui.label(hint_text("token_budget"));
                    ui.add(egui::TextEdit::singleline(&mut profile.token_budget).desired_width(70.0).hint_text("none"))
                        .on_hover_text("Provider tokens-per-minute cap; max_tokens shrinks per request to fit");
                });
                ui.end_row();
            }
        });
    ui.add_space(space::MD);
    ui.horizontal(|ui| {
        let testing = matches!(test_state, ProfileTestView::Running);
        if ui
            .add_enabled(!testing, small_button("Test connection"))
            .on_hover_text("Sends one tiny request with these settings")
            .clicked()
        {
            *action = SettingsAction::TestProfile(i);
        }
        match test_state {
            ProfileTestView::Idle => {}
            ProfileTestView::Running => {
                ui.spinner();
                ui.label(hint_text("testing\u{2026}"));
            }
            ProfileTestView::Done(Ok(msg)) => {
                ui.label(egui::RichText::new(msg).size(font::CAPTION).color(color::SUCCESS));
            }
            ProfileTestView::Done(Err(msg)) => {
                ui.label(egui::RichText::new(msg).size(font::CAPTION).color(color::DANGER));
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_enabled(count > 1, small_button("Remove profile"))
                .on_hover_text(if count > 1 { "Removed on Save" } else { "The last profile cannot be removed" })
                .clicked()
            {
                form.profiles.remove(i);
                form.editing = None;
                if form.default_model == i {
                    form.default_model = 0;
                } else if form.default_model > i {
                    form.default_model -= 1;
                }
            }
        });
    });
    ui.add_space(space::LG);
    render_settings_footer(ui, form, action, dirty);
}












/// The text column under an optional `HeightTarget`: floors it at the
/// target's remainder (`reserved_after` = what still follows it), and with
/// `cap` also scrolls it there; no target = natural height up to
/// `MAX_RESULT_HEIGHT`. `set_min_height` reserves from the current cursor, so
/// the floor is scoped to exactly this column.
fn render_text_column(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    text: &str,
    target: Option<HeightTarget>,
    reserved_after: f32,
    stick_to_bottom: bool,
) {
    match target {
        Some(t) => {
            let floor = t.remaining(ui, reserved_after);
            let max_height = if t.cap { floor } else { MAX_RESULT_HEIGHT };
            ui.scope(|ui| {
                ui.set_min_height(floor);
                scroll_text(ui, id_salt, text, max_height, stick_to_bottom);
            });
        }
        None => scroll_text(ui, id_salt, text, MAX_RESULT_HEIGHT, stick_to_bottom),
    }
}





/// Render the Capturing state: a spinner shown immediately on double-tap while the
/// selection is copied on a background thread (no content/tabs yet).
fn render_capturing(
    ui: &mut egui::Ui,
    picking_text: Option<&str>,
    source: CaptureSource,
    elapsed: Option<std::time::Duration>,
    target: Option<HeightTarget>,
    action: &mut OverlayAction,
) {
    if let Some(text) = picking_text {
        // Single-tap picking: the clipboard content has arrived, so show the
        // data that will be processed in the chosen mode on release.
        render_text_column(ui, "picking", text, target, 4.0 + size::ACTION_BTN, false);
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
                    .color(color::TEXT)
                    .size(font::BODY),
            );
            elapsed_label(ui, elapsed);
        });
    }
    ui.add_space(space::SM);
    if cancel_button(ui) {
            *action = OverlayAction::Cancel;
        }
}

#[allow(clippy::too_many_arguments)]
fn render_processing(
    ui: &mut egui::Ui,
    mode: ProcessMode,
    streaming_text: &str,
    think_started: bool,
    retry_notice: Option<&str>,
    elapsed: Option<std::time::Duration>,
    target: Option<HeightTarget>,
    action: &mut OverlayAction,
) {
    // Top row: shared slot with Result's think toggle (see `size::ROW`)
    // — whichever of these variants is showing on the last Processing
    // frame, it occupies the same height as Result's row that replaces it.
    fixed_height_row(ui, size::ROW, |ui| {
        if let Some(notice) = retry_notice {
            // Automatic retry pending: a silent retry is indistinguishable
            // from a slow first attempt, so say so (amber = degraded, not failed).
            ui.spinner();
            ui.label(
                egui::RichText::new(notice)
                    .color(color::WARNING)
                    .size(font::BODY),
            );
            elapsed_label(ui, elapsed);
        } else if think_started && streaming_text.is_empty() {
            // Think block in progress, no visible output yet.
            ui.spinner();
            ui.label(
                egui::RichText::new("Thinking...")
                    .color(color::TEXT_SECONDARY)
                    .size(font::BODY),
            );
            elapsed_label(ui, elapsed);
        } else if think_started {
            // Think done, answer streaming: show locked collapsed header.
            ui.label(
                egui::RichText::new("\u{25b6} Thinking")
                    .color(color::TEXT_MUTED)
                    .size(font::LABEL),
            );
            elapsed_label(ui, elapsed);
        } else {
            ui.spinner();
            ui.label(
                egui::RichText::new(mode.processing_label())
                    .color(color::TEXT)
                    .size(font::BODY),
            );
            elapsed_label(ui, elapsed);
        }
    });
    if !streaming_text.is_empty() {
        ui.add_space(space::SM);
        render_text_column(
            ui,
            ("streaming", mode),
            streaming_text,
            target,
            4.0 + size::ACTION_BTN,
            true,
        );
    }
    ui.add_space(space::SM);
    // Bottom row: shared slot with Result's reserved action-button space (see
    // `size::ACTION_BTN`).
    fixed_height_row(ui, size::ACTION_BTN, |ui| {
        if cancel_button(ui) {
            *action = OverlayAction::Cancel;
        }
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
            .color(color::DANGER)
            .size(font::LABEL),
    );
    // Bottom controls row, same slot and docked-button style as Result's (see
    // `size::ACTION_BTN`): retry at the far right, copy-debug to its left —
    // matching their relative order in Result, which only adds the primary
    // copy/paste button after them.
    if can_retry || debug_available {
        ui.add_space(space::SM);
        fixed_height_row(ui, size::ACTION_BTN, |ui| {
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
    source_files: &[String],
    // Compact completion summary for the bottom row (see the doc comment on
    // `render()`'s `completion_status` parameter, which this is threaded
    // from).
    completion_status: Option<&str>,
    model_switchable: bool,
    // Height target from `render()` (latch floor, or the user's capped size;
    // already applied as a floor at the true top of the ui there). Its
    // remainder budgets the answer text column below. A latch (`cap ==
    // false`) is ignored while `think_expanded`, which is free to grow the
    // window; collapsing again returns to at least the latched height.
    target: Option<HeightTarget>,
    action: &mut OverlayAction,
) {
    // Truncation / interruption banner at the very top: the partial reply is
    // shown below, so tell the user it is incomplete and why (#65).
    if let Some(reason) = incomplete {
        ui.label(
            egui::RichText::new(format!("\u{26a0} Incomplete — {reason}"))
                .color(color::WARNING)
                .size(font::LABEL),
        );
        ui.add_space(space::SM);
    }

    // Top row: shared slot with Processing's status/thinking row (see
    // `size::ROW`) — but only when there's an actual think toggle to
    // show. Reserving this row unconditionally (even blank) read as an empty
    // hole above the text, which is worse than the row's height differing
    // from Processing's for the (very common) non-thinking case; a plain
    // result's text instead starts right under the separator.
    if think_content.is_some() {
        fixed_height_row(ui, size::ROW, |ui| {
            if think_toggle(ui, think_expanded) {
                *action = OverlayAction::ToggleThink;
            }
        });
    }
    // Expanded think content is deliberate, user-triggered growth — kept
    // outside the fixed slot above (unaffected styling/size — #6).
    if think_expanded && let Some(content) = think_content {
        think_block(ui, content);
    }
    ui.add_space(space::SM);

    // Answer text. With a latch (and Think collapsed) the leftover budget is
    // a FLOOR only: a short answer still owns the latched space (no gap above
    // the bottom row, no shrink-jump at the Processing→Result seam), while a
    // taller answer grows the window — capping at the latch left the window
    // shorter than its content whenever the last Processing frame undershot
    // the final answer (thinking-only streams, cached/fast responses). A
    // user size caps as well, expanded Think included: the text scrolls in
    // whatever the fixed panel has left. The budget is measured from the
    // cursor, so the optional banner/think rows above are accounted for;
    // `4.0 + size::ACTION_BTN` is what unconditionally follows the text.
    let text_target = target.filter(|t| t.cap || !think_expanded);
    render_text_column(ui, ("result", mode), text, text_target, 4.0 + size::ACTION_BTN, false);

    // Bottom row: shared slot with Processing's Cancel-button row (see
    // `size::ACTION_BTN`): the passive completion summary on the left, the
    // docked action buttons right-aligned in the otherwise-empty right side —
    // "controls swap in place" the way the top row already does.
    ui.add_space(space::SM);
    fixed_height_row(ui, size::ACTION_BTN, |ui| {
        render_source_badge(ui, source, source_files);
        if let Some(status) = completion_status {
            let text = egui::RichText::new(status).color(color::TEXT_MUTED).size(font::CAPTION);
            if model_switchable {
                // The label names the model that answered, so it doubles as
                // the "ask another model" control when profiles exist.
                let resp = ui
                    .add(egui::Label::new(text).sense(egui::Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Switch to the next model profile and re-run");
                if resp.clicked() {
                    *action = OverlayAction::CycleModel;
                }
            } else {
                ui.label(text);
            }
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
fn render_source_badge(ui: &mut egui::Ui, source: CaptureSource, files: &[String]) {
    let (icon, tip) = if files.is_empty() {
        match source {
            CaptureSource::Selection => ("\u{2702}", "Source: selection (double-tap)".to_string()),
            CaptureSource::Clipboard => ("\u{1f4cb}", "Source: clipboard (single-tap)".to_string()),
        }
    } else {
        ("\u{1f4c4}", format!("Source: {} file(s) \u{2014} {}", files.len(), files.join(", ")))
    };
    ui.label(
        egui::RichText::new(icon)
            .size(font::CAPTION)
            .color(color::TEXT_MUTED),
    )
    .on_hover_text(tip);
}



fn render_rephrase_params(
    ui: &mut egui::Ui,
    params: RephraseParams,
    action: &mut OverlayAction,
) {
    // Capture the outer left edge before indent shifts the cursor.
    let outer_left = ui.cursor().min.x;

    let response = ui.indent(egui::Id::new("rephrase_params"), |ui| {
        if let Some(style) = pill_row(ui, "Style", RephraseStyle::ALL, params.style, |s| s.label()) {
            *action = OverlayAction::ChangeRephraseStyle(style);
        }
        if let Some(length) = pill_row(ui, "Length", RephraseLength::ALL, params.length, |l| l.label()) {
            *action = OverlayAction::ChangeRephraseLength(length);
        }
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
        color::ACCENT_DIM,
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
                .size(font::LABEL)
                .color(if !is_available {
                    color::TEXT_DISABLED
                } else if is_selected {
                    color::TEXT
                } else {
                    color::TEXT_MUTED
                });

            let button = egui::Button::new(text)
                .fill(egui::Color32::TRANSPARENT)
                .stroke(egui::Stroke::NONE)
                .corner_radius(size::RADIUS);

            let response = ui.add(button);
            // Explain why a tab is disabled (an image-only clipboard locks out the
            // modes that cannot consume images) instead of silently swallowing
            // the click.
            let response = if !is_available && has_content {
                response.on_hover_text("Requires text — image-only clipboard")
            } else {
                response
            };
            let underline_color = if is_selected {
                // Distinguish the uncommitted cycling preview from the committed mode.
                if cycling {
                    Some(color::ACCENT_PREVIEW)
                } else {
                    Some(color::ACCENT)
                }
            } else if response.hovered() && is_available {
                Some(color::ACCENT_DIM)
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
                    .size(font::LABEL)
                    .color(color::TEXT_SECONDARY),
            )
            .fill(egui::Color32::TRANSPARENT)
            .stroke(egui::Stroke::NONE)
            .corner_radius(size::RADIUS_SM);
            if ui.add(close).on_hover_text("Close (Esc)").clicked() {
                *action = OverlayAction::Close;
            }

            // Pin button (left of close) — when lit, the overlay stays open on
            // focus loss instead of auto-hiding.
            let pin = egui::Button::new(
                egui::RichText::new("\u{1F4CC}").size(font::LABEL).color(if pinned {
                    color::TEXT
                } else {
                    color::TEXT_SECONDARY
                }),
            )
            .fill(if pinned {
                color::SURFACE_RAISED
            } else {
                egui::Color32::TRANSPARENT
            })
            .stroke(egui::Stroke::NONE)
            .corner_radius(size::RADIUS_SM);
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
                        .size(font::MICRO)
                        .color(if is_selected {
                            color::TEXT
                        } else {
                            color::TEXT_MUTED
                        });

                    let button = egui::Button::new(text)
                        .fill(if is_selected {
                            color::SURFACE_HOVER
                        } else {
                            egui::Color32::TRANSPARENT
                        })
                        .corner_radius(size::RADIUS_SM);

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

    /// The settings panel renders headlessly with a real form, sizes itself to
    /// the overlay width, and reports no action on an idle frame.
    #[test]
    fn settings_panel_renders_and_sizes() {
        let ctx = egui::Context::default();
        let mut form = crate::settings::SettingsForm::from_config(&crate::config::Config::default(), None);
        form.error = Some("Both language names are required.".into());
        let mut result = None;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            result = Some(render_settings(
                ctx,
                &mut form,
                None,
                Some("/tmp/config.toml"),
                |_| ProfileTestView::Idle,
                |_| None,
            ));
        });
        let (action, output) = result.unwrap();
        assert_eq!(action, SettingsAction::None);
        let size = output.desired_size.expect("panel must report a size");
        assert!(size.x >= size::SETTINGS_WIDTH, "{size:?}");
        assert!(size.y > 200.0, "form rows must occupy real height: {size:?}");
        assert_eq!(form.default_model, 0, "rendering must not mutate the form");
    }

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
        render_headless_sized(ctx, state, text, min_result_height, None)
    }

    fn render_headless_sized(
        ctx: &egui::Context,
        state: &OverlayState,
        text: &str,
        min_result_height: Option<f32>,
        user_size: Option<egui::Vec2>,
    ) -> OverlayOutput {
        let mut output = None;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            output = Some(render(
                state,
                ProcessMode::Translate,
                StreamingState {
                    text,
                    think_started: false,
                    retry_notice: None,
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
                &[],
                false,
                None,
                false,
                None,
                false,
                min_result_height,
                user_size,
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

    /// A user size fixes the panel: a 200-line Result and a one-word Result
    /// both render at exactly that panel size (the text scrolls inside it),
    /// and so does a streaming Processing frame — the latch is moot.
    #[test]
    fn user_size_fixes_the_panel_in_every_state() {
        let ctx = egui::Context::default();
        let size = egui::vec2(640.0, 420.0);
        let long_text = "line\n".repeat(200);
        let cases: [(OverlayState, &str); 3] = [
            (OverlayState::Result(long_text.clone()), ""),
            (OverlayState::Result("short".into()), ""),
            (OverlayState::Processing, long_text.as_str()),
        ];
        for (state, streaming) in &cases {
            ctx.memory_mut(|m| m.reset_areas());
            let mut out = None;
            for _ in 0..5 {
                out = Some(render_headless_sized(&ctx, state, streaming, Some(300.0), Some(size)));
            }
            let content = out.unwrap().content_size.unwrap();
            assert!(
                (content.x - size.x).abs() < 1.0 && (content.y - size.y).abs() < 1.0,
                "{state:?}: panel {content:?} must match the user size {size:?}",
            );
        }
    }

    /// The grip never reports a size below `size::MIN_PANEL`, and a delta of
    /// zero reports nothing (no redundant resize events while merely holding).
    #[test]
    fn user_size_below_minimum_grows_the_panel_to_the_minimum() {
        let ctx = egui::Context::default();
        let tiny = egui::vec2(100.0, 40.0);
        let mut out = None;
        for _ in 0..5 {
            out = Some(render_headless_sized(&ctx, &OverlayState::Result("x".into()), "", None, Some(tiny)));
        }
        let content = out.unwrap().content_size.unwrap();
        assert!(
            content.x >= size::MIN_PANEL.x - 1.0 && content.y >= tiny.y,
            "the chrome must not collapse below its own size: {content:?}",
        );
    }
}
