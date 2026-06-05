#![deny(unused_must_use)]

pub mod api;
pub mod clipboard;
pub mod config;
pub mod coordinator;
pub use clipboard::ClipboardContent;
#[cfg(feature = "diagnostics")]
pub mod diagnostics;
pub mod hotkey;
pub mod platform;
pub mod ui;
pub mod worker;

use thiserror::Error;

// -- Language constants --
//
// Default language names. Crate-internal: callers must read the runtime values
// via `config::get().primary_lang()` / `secondary_lang()` so external
// config overrides are honored, rather than hardcoding these defaults.

pub(crate) const PRIMARY_LANG: &str = "Korean";
pub(crate) const SECONDARY_LANG: &str = "English";

// -- Rephrase parameters --

/// Style axis for Rephrase mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RephraseStyle {
    /// Fix errors only, preserve original tone and style exactly.
    #[default]
    Correct,
    /// Friendly, conversational tone.
    Casual,
    /// Polite, formal register.
    Formal,
    /// Concise professional business tone.
    Business,
    /// Precise technical/engineering terminology.
    Technical,
}

impl RephraseStyle {
    pub const ALL: &[Self] = &[
        Self::Correct,
        Self::Casual,
        Self::Formal,
        Self::Business,
        Self::Technical,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Correct => "Correct",
            Self::Casual => "Casual",
            Self::Formal => "Formal",
            Self::Business => "Business",
            Self::Technical => "Technical",
        }
    }
}

/// Length axis for Rephrase mode (5 discrete levels).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RephraseLength {
    /// ~40% of original — essential points only.
    Terse,
    /// ~70% of original — remove redundancy.
    Brief,
    /// Keep original length.
    #[default]
    Same,
    /// ~150% of original — additional context or detail.
    Detailed,
    /// ~200% of original — thorough explanation.
    Full,
}

impl RephraseLength {
    pub const ALL: &[Self] = &[
        Self::Terse,
        Self::Brief,
        Self::Same,
        Self::Detailed,
        Self::Full,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Terse => "Terse",
            Self::Brief => "Brief",
            Self::Same => "Same",
            Self::Detailed => "Detailed",
            Self::Full => "Full",
        }
    }
}

// -- Thinking mode --

/// Per-mode thinking control: explicitly enable or disable thinking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThinkingMode {
    Think,
    NoThink,
}

impl ThinkingMode {
    pub const ALL: &[Self] = &[Self::Think, Self::NoThink];

    pub fn label(self) -> &'static str {
        match self {
            Self::Think => "Think",
            Self::NoThink => "No Think",
        }
    }
}

/// Bundled rephrase parameters — passed as a single argument instead of (style, length) pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RephraseParams {
    pub style: RephraseStyle,
    pub length: RephraseLength,
}

// -- Process mode --

/// Available processing modes for the LLM pipeline.
/// Add new variants here and to `ALL` to extend the tab bar automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ProcessMode {
    #[default]
    Translate,
    Rephrase,
    Summarize,
}

impl ProcessMode {
    /// All modes in tab bar display order.
    pub const ALL: &[ProcessMode] = &[
        ProcessMode::Translate,
        ProcessMode::Rephrase,
        ProcessMode::Summarize,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Translate => "Translate",
            Self::Rephrase => "Rephrase",
            Self::Summarize => "Summarize",
        }
    }

    pub fn processing_label(self) -> &'static str {
        match self {
            Self::Translate => "Translating...",
            Self::Rephrase => "Rephrasing...",
            Self::Summarize => "Summarizing...",
        }
    }

    /// Default thinking mode for this processing mode.
    pub fn default_thinking(self) -> ThinkingMode {
        match self {
            Self::Translate | Self::Rephrase => ThinkingMode::NoThink,
            Self::Summarize => ThinkingMode::Think,
        }
    }

    /// Returns the processing label, using style-aware label for Rephrase mode.
    pub fn processing_label_rephrase(self, params: RephraseParams) -> &'static str {
        if self == Self::Rephrase && params.style == RephraseStyle::Correct {
            "Correcting..."
        } else {
            self.processing_label()
        }
    }

    /// Builds the system prompt for this mode from the runtime
    /// [`Config`](crate::config::Config). Templates and language
    /// names come from the external config when present, else built-in defaults.
    pub fn system_prompt(self, params: RephraseParams, image_only: bool) -> String {
        let config = crate::config::get();
        let primary = config.primary_lang();
        let secondary = config.secondary_lang();
        match self {
            Self::Translate => {
                crate::config::substitute(config.translate_prompt(), primary, secondary)
            }
            Self::Rephrase => config.rephrase_prompt(params.style, params.length),
            Self::Summarize if image_only => {
                crate::config::substitute(config.summarize_image_prompt(), primary, secondary)
            }
            Self::Summarize => {
                crate::config::substitute(config.summarize_prompt(), primary, secondary)
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("accessibility permission required")]
    AccessibilityDenied,

    #[error("copy simulation failed: {0}")]
    CopyFailed(String),

    #[error("paste simulation failed: {0}")]
    PasteFailed(String),
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("empty response from model")]
    EmptyResponse,

    #[error("no usable content: image-only clipboard but model lacks vision support")]
    NoUsableContent,

    #[error("request cancelled")]
    Cancelled,
}

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("clipboard access failed: {0}")]
    AccessFailed(String),

    #[error("no text in clipboard")]
    NoTextInClipboard,

    #[error("no text in clipboard after copy simulation")]
    NoTextAfterCopy,

    #[error("clipboard write failed: {0}")]
    WriteFailed(String),

    #[error("image encoding failed: {0}")]
    ImageEncodeFailed(String),

    #[error("copy simulation failed: {0}")]
    CopyFailed(#[from] PlatformError),
}

#[derive(Debug, Error)]
pub enum HotkeyError {
    #[error("failed to initialize hotkey manager: {0}")]
    InitFailed(String),

    #[error("failed to register hotkey: {0}")]
    RegisterFailed(String),
}

