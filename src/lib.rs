#![deny(unused_must_use)]

pub mod api;
pub mod clipboard;
#[cfg(feature = "diagnostics")]
pub mod diagnostics;
pub mod hotkey;
pub mod platform;
pub mod ui;
pub mod worker;

use thiserror::Error;

// -- Process mode --

/// Available processing modes for the LLM pipeline.
/// Add new variants here and to `ALL` to extend the tab bar automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessMode {
    #[default]
    Translate,
    Correct,
}

impl ProcessMode {
    /// All modes in tab bar display order.
    pub const ALL: &[ProcessMode] = &[ProcessMode::Translate, ProcessMode::Correct];

    pub fn label(self) -> &'static str {
        match self {
            Self::Translate => "Translate",
            Self::Correct => "Correct",
        }
    }

    pub fn processing_label(self) -> &'static str {
        match self {
            Self::Translate => "Translating...",
            Self::Correct => "Correcting...",
        }
    }

    pub fn system_prompt(self) -> &'static str {
        match self {
            Self::Translate => "\
You are a Korean↔English translator for software engineering text. \
Auto-detect the input language: if Korean, translate to English; if English, translate to Korean. \
Rules: \
- If the input contains code: preserve all whitespace, indentation, and structure exactly. Never dedent or normalize. Do not translate code, variable names, or identifiers — only translate comments and string literals. \
- If the input is plain text: translate naturally while keeping the general structure. \
- Output the translation only — no preamble, labels, explanations, or markdown formatting.",
            Self::Correct => "\
You are a proofreader for software engineering text. \
Auto-detect the input language and correct it in the same language. \
Fix grammar, spelling, punctuation, and awkward phrasing to improve naturalness while preserving the original meaning and tone. \
Rules: \
- If the input contains code: preserve all whitespace, indentation, and structure exactly. Never dedent or normalize. Do not modify code, variable names, or identifiers — only correct comments and string literals. \
- If the input is plain text: correct naturally while keeping the general structure. \
- Output the corrected text only — no preamble, labels, explanations, or markdown formatting.",
        }
    }
}

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("accessibility permission required")]
    AccessibilityDenied,

    #[error("copy simulation failed: {0}")]
    CopyFailed(String),
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("unexpected response structure: {0}")]
    ParseError(String),

    #[error("empty response from model")]
    EmptyResponse,

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

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Platform(#[from] PlatformError),

    #[error(transparent)]
    Clipboard(#[from] ClipboardError),

    #[error(transparent)]
    Api(#[from] ApiError),

    #[error(transparent)]
    Hotkey(#[from] HotkeyError),
}
