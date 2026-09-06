//! Design tokens — the single source of truth for the overlay's type scale,
//! palettes (dark and light), spacing and geometry (docs/UI-GUIDELINES.md §3).
//! Views reference these roles; a literal size or color outside this module
//! is a bug.

use eframe::egui::{self, Color32};

/// Type scale (points).
pub mod font {
    /// Settings title.
    pub const TITLE: f32 = 16.0;
    /// Content: picked text, the answer, error messages.
    pub const BODY: f32 = 15.0;
    /// Tabs, every status-row label, the Think toggle, form row labels.
    pub const LABEL: f32 = 13.0;
    /// Pills, footer status, hints, text buttons.
    pub const CAPTION: f32 = 12.0;
    /// Section headers, the thinking pills in the header.
    pub const MICRO: f32 = 11.0;
}

/// Palette by role. Two palettes exist, dark and light; which one the
/// functions return follows the egui theme (`[ui].theme`), selected once per
/// frame by `apply` before anything is drawn.
pub mod color {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{egui, Color32};

    static LIGHT: AtomicBool = AtomicBool::new(false);

    /// Select this frame's palette from the context's resolved theme.
    pub fn apply(ctx: &egui::Context) {
        LIGHT.store(ctx.theme() == egui::Theme::Light, Ordering::Relaxed);
    }

    fn pick(dark: Color32, light: Color32) -> Color32 {
        if LIGHT.load(Ordering::Relaxed) { light } else { dark }
    }

    const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color32 {
        Color32::from_rgba_unmultiplied_const(r, g, b, a)
    }

    // Text tones.
    /// Content and selected controls.
    pub fn text() -> Color32 {
        pick(Color32::WHITE, Color32::from_gray(25))
    }
    /// Form labels.
    pub fn text_soft() -> Color32 {
        pick(Color32::from_gray(200), Color32::from_gray(60))
    }
    /// Idle interactive controls (icons, text buttons).
    pub fn text_secondary() -> Color32 {
        pick(Color32::from_gray(165), Color32::from_gray(95))
    }
    /// Captions, hints, unselected tabs, Think content.
    pub fn text_muted() -> Color32 {
        pick(Color32::from_gray(130), Color32::from_gray(120))
    }
    /// Unavailable controls, the idle grip.
    pub fn text_disabled() -> Color32 {
        pick(Color32::from_gray(95), Color32::from_gray(175))
    }

    // Accent.
    /// Selected-tab underline.
    pub fn accent() -> Color32 {
        pick(rgba(108, 166, 255, 200), rgba(30, 100, 220, 220))
    }
    /// Uncommitted cycling preview underline — between dim and full.
    pub fn accent_preview() -> Color32 {
        pick(rgba(108, 166, 255, 140), rgba(30, 100, 220, 150))
    }
    /// Hover underline, the rephrase indent rule.
    pub fn accent_dim() -> Color32 {
        pick(rgba(108, 166, 255, 80), rgba(30, 100, 220, 90))
    }
    /// Fill of a selected accent pill (an explicit choice).
    pub fn accent_fill() -> Color32 {
        pick(rgba(70, 95, 140, 220), rgba(170, 200, 245, 230))
    }

    // Semantic.
    /// Errors, Cancel.
    pub fn danger() -> Color32 {
        pick(Color32::from_rgb(255, 110, 110), Color32::from_rgb(200, 40, 40))
    }
    /// Behind Cancel.
    pub fn danger_fill() -> Color32 {
        pick(rgba(80, 30, 30, 180), rgba(250, 205, 205, 220))
    }
    /// Degraded but running: retry pending, incomplete answer.
    pub fn warning() -> Color32 {
        pick(Color32::from_rgb(240, 175, 60), Color32::from_rgb(170, 110, 0))
    }
    /// Confirmations.
    pub fn success() -> Color32 {
        pick(Color32::from_rgb(120, 200, 140), Color32::from_rgb(25, 130, 65))
    }

    // Surfaces.
    /// The panel frame.
    pub fn surface() -> Color32 {
        pick(rgba(30, 30, 30, 230), rgba(248, 248, 248, 235))
    }
    /// The settings panel frame: `surface` without the translucency, so the
    /// form stays legible over any desktop.
    pub fn surface_opaque() -> Color32 {
        pick(Color32::from_rgb(30, 30, 30), Color32::from_rgb(248, 248, 248))
    }
    /// A selected neutral control (pin, quiet pill, param pill).
    pub fn surface_raised() -> Color32 {
        pick(rgba(60, 60, 60, 200), rgba(205, 205, 205, 220))
    }
    /// A hovered docked button.
    pub fn surface_hover() -> Color32 {
        pick(rgba(50, 50, 50, 200), rgba(215, 215, 215, 220))
    }
    /// An idle pill or docked button.
    pub fn surface_subtle() -> Color32 {
        pick(rgba(50, 50, 50, 110), rgba(215, 215, 215, 120))
    }
    /// Separator rules inside the settings form.
    pub fn rule() -> Color32 {
        pick(Color32::from_gray(55), Color32::from_gray(205))
    }
    /// The frame's drop shadow.
    pub fn shadow() -> Color32 {
        pick(Color32::from_black_alpha(100), Color32::from_black_alpha(60))
    }
}

/// Gaps between elements.
pub mod space {
    pub const XS: f32 = 2.0;
    pub const SM: f32 = 4.0;
    pub const MD: f32 = 8.0;
    pub const LG: f32 = 10.0;
}

/// Fixed dimensions.
pub mod size {
    use super::egui;

    /// Panel (frame incl. margin) when `[ui].panel_size` is unset.
    pub const DEFAULT_PANEL: egui::Vec2 = egui::vec2(512.0, 380.0);
    /// Smallest panel the grip allows: header row plus one text line plus
    /// the footer still fit.
    pub const MIN_PANEL: egui::Vec2 = egui::vec2(440.0, 180.0);
    /// Content width of the settings form (its height follows the form).
    pub const SETTINGS_WIDTH: f32 = 560.0;
    /// Transparent border around the frame where its shadow is drawn.
    pub const SHADOW_PAD: f32 = 20.0;
    /// Frame inner margin (horizontal, vertical).
    pub const FRAME_MARGIN: egui::Margin = egui::Margin::symmetric(16, 14);
    pub const FRAME_RADIUS: u8 = 12;
    /// Status rows (spinner + label, Think toggle) and pills.
    pub const ROW: f32 = 24.0;
    /// Docked action buttons; also the footer height.
    pub const ACTION_BTN: f32 = 26.0;
    /// Narrowest revise input worth drawing; below it the footer keeps only
    /// its buttons.
    pub const REVISE_INPUT_MIN: f32 = 80.0;
    /// Chars of the last revision instruction shown in the status row.
    pub const REVISION_LABEL_CHARS: usize = 24;
    /// Side of the resize grip's hit area.
    pub const GRIP: f32 = 16.0;
    /// Cap for the expanded Think block inside the body.
    pub const THINK_MAX_HEIGHT: f32 = 120.0;
    /// Smallest text column ever laid out (guards a degenerate budget).
    pub const MIN_TEXT_HEIGHT: f32 = 24.0;
    /// Height of the gradient that marks text continuing past a scroll edge.
    pub const FADE_HEIGHT: f32 = 28.0;
    /// Icon buttons.
    pub const RADIUS_SM: f32 = 4.0;
    /// Pills and text buttons.
    pub const RADIUS: f32 = 6.0;
}

/// `RichText` in a type role and tone — the only way views make text.
pub fn text(s: impl Into<String>, size: f32, color: Color32) -> egui::RichText {
    egui::RichText::new(s).size(size).color(color)
}

/// One-time egui style setup for both themes (`[ui].theme` picks one later):
/// a transparent viewport for the overlay, and light-widget contrast fixes.
pub fn configure(ctx: &egui::Context) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            style.visuals.window_fill = egui::Color32::TRANSPARENT;
            style.visuals.panel_fill = egui::Color32::TRANSPARENT;
            style.visuals.window_stroke = egui::Stroke::NONE;
            style.visuals.window_shadow = egui::Shadow::NONE;
            style.visuals.window_corner_radius = egui::CornerRadius::same(12);
            if theme == egui::Theme::Light {
                // egui's light widgets assume a white window; on the
                // overlay's light-grey frame their near-white fills and
                // strokes (radio rings, slider trough, combo border)
                // disappear. Pull them a few steps darker.
                let v = &mut style.visuals;
                v.extreme_bg_color = egui::Color32::from_gray(225);
                v.faint_bg_color = egui::Color32::from_gray(232);
                v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(180));
                v.widgets.inactive.bg_fill = egui::Color32::from_gray(205);
                v.widgets.inactive.weak_bg_fill = egui::Color32::from_gray(215);
                v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_gray(160));
                v.widgets.hovered.bg_fill = egui::Color32::from_gray(190);
                v.widgets.hovered.weak_bg_fill = egui::Color32::from_gray(200);
                v.widgets.active.bg_fill = egui::Color32::from_gray(175);
                v.widgets.active.weak_bg_fill = egui::Color32::from_gray(185);
            }
        });
    }
}
