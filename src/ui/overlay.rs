use eframe::egui;

use super::panel::{self, GripAction, Slots};
use super::state_machine::{CaptureSource, OverlayState};
use super::theme::{self, color, font, size, space};
use super::widgets::{
    cancel_button, docked_action_button, hint_text, language_picker, pill,
    pill_row, pill_styled, pill_with_tip, row_label, section_header, small_button, status_row,
    think_block, think_toggle, PillTone,
};
use crate::{ProcessMode, RephraseLength, RephraseParams, RephraseStyle, ThinkingMode};

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

/// Render the overlay panel for `state`: the fixed-size frame, the shared
/// header, and the state's body/footer (docs/UI-GUIDELINES.md). Returns the
/// user's action and the window geometry.
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
    // Compact completion summary ("✓ 2.4s · 850 tokens") for Result's footer;
    // `None` for a cached/instant result (see `format_completion_status`).
    completion_status: Option<String>,
    // More than one model profile exists: the status label switches models.
    model_switchable: bool,
    // Panel (frame incl. margin) size: the config default or the grip's
    // last value. Content never changes it.
    panel_size: egui::Vec2,
    ctx: &egui::Context,
) -> OverlayOutput {
    if matches!(state, OverlayState::Hidden) {
        return OverlayOutput { action: OverlayAction::None, desired_size: None, content_size: None };
    }

    let mut action = OverlayAction::None;
    let capturing = matches!(state, OverlayState::Capturing);
    let out = panel::show(ctx, panel_size, |slots| {
        slots.header(|ui| {
            // While capturing the content type is unknown, so every mode is
            // offered; image-only is reconciled in on_content_ready.
            let modes = if capturing { ProcessMode::display_order() } else { available_modes };
            render_tab_bar(ui, mode, modes, thinking, pinned, preview_mode, &mut action);
            if mode == ProcessMode::Rephrase && !capturing {
                ui.add_space(space::SM);
                render_rephrase_params(ui, rephrase_params, &mut action);
            }
        });
        match state {
            OverlayState::Capturing => {
                view_capturing(slots, picking_text, source, elapsed, &mut action)
            }
            OverlayState::Processing => view_processing(slots, mode, &streaming, elapsed, &mut action),
            OverlayState::Result(text) => view_result(
                slots,
                mode,
                text,
                &streaming,
                ResultFooter {
                    auto_copy,
                    copy_confirmed,
                    debug_available,
                    source,
                    source_files,
                    completion_status: completion_status.as_deref(),
                    model_switchable,
                },
                &mut action,
            ),
            // Retry needs loaded content; a capture failure leaves none
            // (available_modes is empty), so hide the button.
            OverlayState::Error(msg) => {
                view_error(slots, msg, !available_modes.is_empty(), debug_available, &mut action)
            }
            OverlayState::Hidden => unreachable!("returned above"),
        }
    });

    match out.grip {
        GripAction::Resize(panel) => action = OverlayAction::Resize(panel),
        GripAction::Done => action = OverlayAction::ResizeDone,
        GripAction::Reset => action = OverlayAction::ResetSize,
        GripAction::None => {}
    }
    // Drag the OS window when the user drags the panel background.
    if out.drag_started {
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

    OverlayOutput {
        action,
        desired_size: Some(out.desired_size),
        content_size: Some(out.content_size),
    }
}

/// Footer actions, right-aligned. Render the primary action first: the
/// layout is right-to-left, so it lands at the far right edge.
fn actions_right(ui: &mut egui::Ui, add_actions: impl FnOnce(&mut egui::Ui)) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add_actions);
}

/// Capturing: status names the capture in flight (double-tap copies on
/// modifier release, the clipboard read runs off-thread — #38); the body
/// shows the picked clipboard text once it has arrived. Footer: Cancel.
fn view_capturing(
    slots: &mut Slots<'_>,
    picking_text: Option<&str>,
    source: CaptureSource,
    elapsed: Option<std::time::Duration>,
    action: &mut OverlayAction,
) {
    slots.status(|ui| {
        let label = match (picking_text, source) {
            (Some(_), _) => "Release to process",
            (None, CaptureSource::Selection) => "Copying selection...",
            (None, CaptureSource::Clipboard) => "Reading clipboard...",
        };
        status_row(ui, picking_text.is_none(), theme::text(label, font::BODY, color::TEXT), elapsed);
    });
    slots.body(|body| {
        if let Some(text) = picking_text {
            body.fill_text("picking", text, color::TEXT, false);
        }
    });
    slots.footer(|ui| {
        actions_right(ui, |ui| {
            if cancel_button(ui) {
                *action = OverlayAction::Cancel;
            }
        });
    });
}

/// Processing: status names the phase; the body streams the text, sticking
/// to its bottom. Footer: Cancel.
fn view_processing(
    slots: &mut Slots<'_>,
    mode: ProcessMode,
    streaming: &StreamingState<'_>,
    elapsed: Option<std::time::Duration>,
    action: &mut OverlayAction,
) {
    slots.status(|ui| {
        if let Some(notice) = streaming.retry_notice {
            // A silent retry is indistinguishable from a slow first attempt,
            // so say so (WARNING = degraded, not failed).
            status_row(ui, true, theme::text(notice, font::BODY, color::WARNING), elapsed);
        } else if streaming.think_started && streaming.text.is_empty() {
            status_row(ui, true, theme::text("Thinking...", font::BODY, color::TEXT_MUTED), elapsed);
        } else if streaming.think_started {
            // Think done, answer streaming: the collapsed header Result will
            // show, locked.
            let label = theme::text("\u{25b6} Thinking", font::LABEL, color::TEXT_MUTED);
            status_row(ui, false, label, elapsed);
        } else {
            let label = theme::text(mode.processing_label(), font::BODY, color::TEXT);
            status_row(ui, true, label, elapsed);
        }
    });
    slots.body(|body| {
        if !streaming.text.is_empty() {
            body.fill_text(("streaming", mode), streaming.text, color::TEXT, true);
        }
    });
    slots.footer(|ui| {
        actions_right(ui, |ui| {
            if cancel_button(ui) {
                *action = OverlayAction::Cancel;
            }
        });
    });
}

/// Error: status says it failed; the body is the user-facing message (#27).
/// Footer: Retry, copy-debug.
fn view_error(
    slots: &mut Slots<'_>,
    message: &str,
    can_retry: bool,
    debug_available: bool,
    action: &mut OverlayAction,
) {
    slots.status(|ui| {
        status_row(ui, false, theme::text("\u{2715} Request failed", font::BODY, color::DANGER), None);
    });
    slots.body(|body| body.fill_text("error", message, color::TEXT, false));
    slots.footer(|ui| {
        actions_right(ui, |ui| {
            if can_retry && docked_action_button(ui, "\u{21bb}", "Retry") {
                *action = OverlayAction::Retry;
            }
            if debug_available && copy_debug_button(ui) {
                *action = OverlayAction::CopyDebug;
            }
        });
    });
}

/// Everything Result's footer shows besides the answer itself.
struct ResultFooter<'a> {
    auto_copy: bool,
    copy_confirmed: bool,
    debug_available: bool,
    source: CaptureSource,
    source_files: &'a [String],
    completion_status: Option<&'a str>,
    model_switchable: bool,
}

/// Result: status carries the Think toggle and the completion summary (which
/// switches models when profiles exist); the body is the answer, preceded by
/// the expanded Think block and an "incomplete" banner when they apply.
/// Footer: source badge on the left; primary action, Retry and copy-debug
/// on the right.
fn view_result(
    slots: &mut Slots<'_>,
    mode: ProcessMode,
    text: &str,
    streaming: &StreamingState<'_>,
    footer: ResultFooter<'_>,
    action: &mut OverlayAction,
) {
    slots.status(|ui| {
        if streaming.think_content.is_some() && think_toggle(ui, streaming.think_expanded) {
            *action = OverlayAction::ToggleThink;
        }
        if let Some(status) = footer.completion_status {
            let label = theme::text(status, font::CAPTION, color::TEXT_MUTED);
            if footer.model_switchable {
                // The label names the model that answered, so it doubles as
                // the "ask another model" control when profiles exist.
                let resp = ui
                    .add(egui::Label::new(label).sense(egui::Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("Switch to the next model profile and re-run");
                if resp.clicked() {
                    *action = OverlayAction::CycleModel;
                }
            } else {
                ui.label(label);
            }
        }
    });
    slots.body(|body| {
        // The partial reply is shown below, so say it is incomplete and why (#65).
        if let Some(reason) = streaming.incomplete {
            let banner = format!("\u{26a0} Incomplete — {reason}");
            body.ui.label(theme::text(banner, font::LABEL, color::WARNING));
            body.ui.add_space(space::SM);
        }
        if streaming.think_expanded && let Some(content) = streaming.think_content {
            think_block(body.ui, content);
            body.ui.add_space(space::SM);
        }
        body.fill_text(("result", mode), text, color::TEXT, false);
    });
    slots.footer(|ui| {
        render_source_badge(ui, footer.source, footer.source_files);
        actions_right(ui, |ui| {
            // Primary: auto_copy (double-tap) = paste/replace (↩); otherwise
            // copy (📋), with ✓ confirming a just-done copy (#16a).
            let (icon, tip) = if footer.auto_copy {
                ("\u{21a9}", "Paste over the selection (Enter)")
            } else if footer.copy_confirmed {
                ("\u{2713}", "Copied")
            } else {
                ("\u{1f4cb}", "Copy to clipboard (Enter)")
            };
            if docked_action_button(ui, icon, tip) {
                *action = if footer.auto_copy {
                    OverlayAction::PasteReplace
                } else {
                    OverlayAction::CopyToClipboard
                };
            }
            if docked_action_button(ui, "\u{21bb}", "Retry") {
                *action = OverlayAction::Retry;
            }
            if footer.debug_available && copy_debug_button(ui) {
                *action = OverlayAction::CopyDebug;
            }
        });
    });
}

/// The copy-debug (🔍) action shared by Result and Error.
fn copy_debug_button(ui: &mut egui::Ui) -> bool {
    docked_action_button(ui, "\u{1f50d}", "Copy the raw request + response to the clipboard")
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
            panel::frame().show(ui, |ui| {
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

    /// Render `render()` once inside a headless egui frame (no window, nothing
    /// on screen). Reuses `ctx` so `egui::Area`'s per-frame sizing memory
    /// carries over between calls exactly like consecutive real frames.
    fn render_headless(
        ctx: &egui::Context,
        state: &OverlayState,
        mode: ProcessMode,
        streaming_text: &str,
        panel_size: egui::Vec2,
    ) -> OverlayOutput {
        let mut output = None;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            output = Some(render(
                state,
                mode,
                StreamingState {
                    text: streaming_text,
                    think_started: false,
                    retry_notice: None,
                    think_content: Some("some reasoning"),
                    think_expanded: false,
                    incomplete: Some("token limit"),
                },
                ProcessMode::ALL,
                None,
                None,
                RephraseParams::default(),
                ThinkingState { mode: ThinkingMode::NoThink, supported: true },
                false,
                true,
                CaptureSource::Selection,
                &[],
                false,
                Some(std::time::Duration::from_secs(3)),
                true,
                Some("\u{2713} 2.4s".into()),
                true,
                panel_size,
                ctx,
            ));
        });
        output.expect("render() must run synchronously inside ctx.run's closure")
    }

    /// Render `n` settled frames and return the last one's frame size.
    fn settled_size(state: &OverlayState, mode: ProcessMode, streaming: &str, panel: egui::Vec2) -> egui::Vec2 {
        let ctx = egui::Context::default();
        let mut out = None;
        for _ in 0..5 {
            out = Some(render_headless(&ctx, state, mode, streaming, panel));
        }
        out.unwrap().content_size.expect("visible states report a size")
    }

    fn close(a: egui::Vec2, b: egui::Vec2) -> bool {
        (a.x - b.x).abs() < 1.0 && (a.y - b.y).abs() < 1.0
    }

    /// Geometry policy (UI-GUIDELINES §1): every state renders at exactly the
    /// panel size — a one-word answer, a 200-line answer, a streaming frame, a
    /// long error, the capture spinner, and Rephrase with its extra rows.
    #[test]
    fn every_state_renders_at_the_panel_size() {
        let panel = egui::vec2(640.0, 420.0);
        let long = "line\n".repeat(200);
        let cases: [(OverlayState, ProcessMode, &str); 7] = [
            (OverlayState::Capturing, ProcessMode::Translate, ""),
            (OverlayState::Processing, ProcessMode::Translate, ""),
            (OverlayState::Processing, ProcessMode::Translate, long.as_str()),
            (OverlayState::Result("short".into()), ProcessMode::Translate, ""),
            (OverlayState::Result(long.clone()), ProcessMode::Translate, ""),
            (OverlayState::Result(long.clone()), ProcessMode::Rephrase, ""),
            (OverlayState::Error(long.clone()), ProcessMode::Translate, ""),
        ];
        for (state, mode, streaming) in &cases {
            let size = settled_size(state, *mode, streaming, panel);
            assert!(close(size, panel), "{state:?}/{mode:?}: {size:?} must equal {panel:?}");
        }
    }

    /// A panel smaller than `size::MIN_PANEL` is clamped up to it.
    #[test]
    fn panel_size_is_clamped_to_the_minimum() {
        let size = settled_size(&OverlayState::Result("x".into()), ProcessMode::Translate, "", egui::vec2(100.0, 40.0));
        assert!(close(size, size::MIN_PANEL), "{size:?}");
    }

    /// The same panel size is reported whether or not the state changed: the
    /// window never has to move or resize at a transition.
    #[test]
    fn transitions_keep_the_window_size() {
        let panel = egui::vec2(512.0, 380.0);
        let ctx = egui::Context::default();
        let mut sizes = Vec::new();
        for state in [
            OverlayState::Capturing,
            OverlayState::Processing,
            OverlayState::Result("line\n".repeat(50)),
            OverlayState::Error("boom".into()),
        ] {
            ctx.memory_mut(|m| m.reset_areas());
            for _ in 0..3 {
                let out = render_headless(&ctx, &state, ProcessMode::Translate, "streamed", panel);
                sizes.push(out.desired_size.unwrap());
            }
        }
        let first = sizes[0];
        assert!(sizes.iter().all(|s| close(*s, first)), "{sizes:?}");
        assert!(close(first, panel + egui::Vec2::splat(size::SHADOW_PAD * 2.0)));
    }

    /// An idle frame reports no action and a hidden state no geometry.
    #[test]
    fn idle_frame_reports_no_action() {
        let ctx = egui::Context::default();
        let out = render_headless(&ctx, &OverlayState::Result("x".into()), ProcessMode::Translate, "", size::DEFAULT_PANEL);
        assert!(matches!(out.action, OverlayAction::None));
        let hidden = render_headless(&ctx, &OverlayState::Hidden, ProcessMode::Translate, "", size::DEFAULT_PANEL);
        assert!(hidden.desired_size.is_none() && hidden.content_size.is_none());
    }
}
