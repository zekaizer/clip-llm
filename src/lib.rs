#![deny(unused_must_use)]

pub mod api;
pub mod clipboard;
pub use clipboard::ClipboardContent;
#[cfg(feature = "diagnostics")]
pub mod diagnostics;
pub mod hotkey;
pub mod platform;
pub mod ui;
pub mod worker;

use thiserror::Error;

// -- Language constants --

pub const PRIMARY_LANG: &str = "Korean";
pub const SECONDARY_LANG: &str = "English";

// -- Process mode --

/// Available processing modes for the LLM pipeline.
/// Add new variants here and to `ALL` to extend the tab bar automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ProcessMode {
    #[default]
    Translate,
    Correct,
    Summarize,
}

impl ProcessMode {
    /// All modes in tab bar display order.
    pub const ALL: &[ProcessMode] = &[
        ProcessMode::Translate,
        ProcessMode::Correct,
        ProcessMode::Summarize,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Translate => "Translate",
            Self::Correct => "Correct",
            Self::Summarize => "Summarize",
        }
    }

    pub fn processing_label(self) -> &'static str {
        match self {
            Self::Translate => "Translating...",
            Self::Correct => "Correcting...",
            Self::Summarize => "Summarizing...",
        }
    }

    pub fn system_prompt(self) -> String {
        match self {
            Self::Translate => format!(
                "You are a {PRIMARY_LANG}↔{SECONDARY_LANG} translator for software engineering text. \
                 Auto-detect the input language: if {PRIMARY_LANG}, translate to {SECONDARY_LANG}; \
                 if {SECONDARY_LANG}, translate to {PRIMARY_LANG}. \
                 Rules: \
                 - If the input contains code: preserve all whitespace, indentation, and structure exactly. \
                 Never dedent or normalize. Do not translate code, variable names, or identifiers \
                 — only translate comments and string literals. \
                 - If the input is plain text: translate naturally while keeping the general structure. \
                 - Output the translation only — no preamble, labels, explanations, or markdown formatting."
            ),
            Self::Correct => "\
You are a proofreader for software engineering text. \
Auto-detect the input language and correct it in the same language. \
Fix grammar, spelling, punctuation, and awkward phrasing to improve naturalness while preserving the original meaning and tone. \
Rules: \
- If the input contains code: preserve all whitespace, indentation, and structure exactly. Never dedent or normalize. Do not modify code, variable names, or identifiers — only correct comments and string literals. \
- If the input is plain text: correct naturally while keeping the general structure. \
- Output the corrected text only — no preamble, labels, explanations, or markdown formatting.".to_owned(),
            Self::Summarize => format!(
                "You are a text summarizer for software engineering content. \
                 Produce a concise summary in {PRIMARY_LANG} that captures the key points \
                 and essential information, regardless of the input language. \
                 Rules: \
                 - Always output in {PRIMARY_LANG}. \
                 - Keep technical terms, proper nouns, and code references intact (do not translate them). \
                 - Keep the total output under 1000 characters. \
                 - STRICT: You MUST NOT add ANY information, opinions, examples, implications, or details \
                 that are not explicitly stated in the input. If the input does not mention it, do not include it. \
                 Every sentence in the summary must be directly traceable to the input text. \
                 - Use the following markdown template. Include only sections that are relevant to the input — \
                 omit any section that has no meaningful content:\n\
                 # [Title]\n\
                 \n\
                 > Few-line summary\n\
                 \n\
                 ## Key Points\n\
                 \n\
                 ## Background / Context\n\
                 \n\
                 ## Conclusion / Judgment\n\
                 \n\
                 ## Open Issues\n\
                 \n\
                 ## Action Items\n\
                 \n\
                 ## Change History\n\
                 \n\
                 ## Stakeholders\n\
                 \n\
                 ## Related Documents\n\
                 \n\
                 ## References"
            ),
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
