#![deny(unused_must_use)]

pub mod api;
pub mod clipboard;
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

pub const PRIMARY_LANG: &str = "Korean";
pub const SECONDARY_LANG: &str = "English";

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

    /// Returns the processing label, using style-aware label for Rephrase mode.
    pub fn processing_label_rephrase(self, params: RephraseParams) -> &'static str {
        if self == Self::Rephrase && params.style == RephraseStyle::Correct {
            "Correcting..."
        } else {
            self.processing_label()
        }
    }

    pub fn system_prompt(self, params: RephraseParams) -> String {
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
            Self::Rephrase => {
                let style_modifier = match params.style {
                    RephraseStyle::Correct =>
                        "Fix grammar, spelling, and punctuation. Preserve original tone and style exactly.",
                    RephraseStyle::Casual =>
                        "Rewrite in a friendly, conversational tone. Fix any errors.",
                    RephraseStyle::Formal =>
                        "Rewrite in a polite, formal register. Fix any errors.",
                    RephraseStyle::Business =>
                        "Rewrite in a concise, professional business tone. Fix any errors.",
                    RephraseStyle::Technical =>
                        "Rewrite using precise technical/engineering terminology naturally. Fix any errors.",
                };
                let length_modifier = match params.length {
                    RephraseLength::Terse =>
                        " Reduce length to approximately 40% of the original. Keep only essential points.",
                    RephraseLength::Brief =>
                        " Reduce length to approximately 70% of the original. Remove redundancy.",
                    RephraseLength::Same => "",
                    RephraseLength::Detailed =>
                        " Expand to approximately 150% of the original with additional context or detail.",
                    RephraseLength::Full =>
                        " Expand to approximately 200% of the original with thorough explanation.",
                };
                format!(
                    "You are a proofreader/rewriter for software engineering text. \
                     Auto-detect the input language and output in the same language. \
                     Preserve all code, variable names, and identifiers unchanged. \
                     {style_modifier}{length_modifier} \
                     Output the result only — no preamble, labels, or markdown."
                )
            }
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

