//! Design tokens — the single source of truth for the overlay's type scale,
//! palette, spacing and geometry (docs/UI-GUIDELINES.md §3). Views reference
//! these roles; a literal size or color outside this module is a bug.

use eframe::egui::{self, Color32};

/// Type scale (points).
pub mod font {
    /// Settings title.
    pub const TITLE: f32 = 16.0;
    /// Content: picked text, the answer, error messages, status labels.
    pub const BODY: f32 = 15.0;
    /// Tabs, the Think toggle, form row labels.
    pub const LABEL: f32 = 13.0;
    /// Pills, footer status, hints, text buttons.
    pub const CAPTION: f32 = 12.0;
    /// Section headers, the thinking pills in the header.
    pub const MICRO: f32 = 11.0;
}

/// Palette by role.
pub mod color {
    use super::Color32;

    // Text tones.
    /// Content and selected controls.
    pub const TEXT: Color32 = Color32::WHITE;
    /// Form labels.
    pub const TEXT_SOFT: Color32 = Color32::from_gray(200);
    /// Idle interactive controls (icons, text buttons).
    pub const TEXT_SECONDARY: Color32 = Color32::from_gray(165);
    /// Captions, hints, unselected tabs, Think content.
    pub const TEXT_MUTED: Color32 = Color32::from_gray(130);
    /// Unavailable controls, the idle grip.
    pub const TEXT_DISABLED: Color32 = Color32::from_gray(95);

    // Accent.
    /// Selected-tab underline.
    pub const ACCENT: Color32 = Color32::from_rgba_unmultiplied_const(108, 166, 255, 200);
    /// Uncommitted cycling preview underline — between dim and full.
    pub const ACCENT_PREVIEW: Color32 = Color32::from_rgba_unmultiplied_const(108, 166, 255, 140);
    /// Hover underline, the rephrase indent rule.
    pub const ACCENT_DIM: Color32 = Color32::from_rgba_unmultiplied_const(108, 166, 255, 80);
    /// Fill of a selected accent pill (an explicit choice).
    pub const ACCENT_FILL: Color32 = Color32::from_rgba_unmultiplied_const(70, 95, 140, 220);

    // Semantic.
    /// Errors, Cancel.
    pub const DANGER: Color32 = Color32::from_rgb(255, 110, 110);
    /// Behind Cancel.
    pub const DANGER_FILL: Color32 = Color32::from_rgba_unmultiplied_const(80, 30, 30, 180);
    /// Degraded but running: retry pending, incomplete answer.
    pub const WARNING: Color32 = Color32::from_rgb(240, 175, 60);
    /// Confirmations.
    pub const SUCCESS: Color32 = Color32::from_rgb(120, 200, 140);

    // Surfaces.
    /// The panel frame.
    pub const SURFACE: Color32 = Color32::from_rgba_unmultiplied_const(30, 30, 30, 230);
    /// A selected neutral control (pin, quiet pill, param pill).
    pub const SURFACE_RAISED: Color32 = Color32::from_rgba_unmultiplied_const(60, 60, 60, 200);
    /// A hovered docked button.
    pub const SURFACE_HOVER: Color32 = Color32::from_rgba_unmultiplied_const(50, 50, 50, 200);
    /// An idle pill or docked button.
    pub const SURFACE_SUBTLE: Color32 = Color32::from_rgba_unmultiplied_const(50, 50, 50, 110);
    /// Separator rules inside the settings form.
    pub const RULE: Color32 = Color32::from_gray(55);
    /// The frame's drop shadow.
    pub const SHADOW: Color32 = Color32::from_black_alpha(100);
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
    /// Side of the resize grip's hit area.
    pub const GRIP: f32 = 16.0;
    /// Cap for the expanded Think block inside the body.
    pub const THINK_MAX_HEIGHT: f32 = 120.0;
    /// Smallest text column ever laid out (guards a degenerate budget).
    pub const MIN_TEXT_HEIGHT: f32 = 24.0;
    /// Icon buttons.
    pub const RADIUS_SM: f32 = 4.0;
    /// Pills and text buttons.
    pub const RADIUS: f32 = 6.0;
}

/// `RichText` in a type role and tone — the only way views make text.
pub fn text(s: impl Into<String>, size: f32, color: Color32) -> egui::RichText {
    egui::RichText::new(s).size(size).color(color)
}
