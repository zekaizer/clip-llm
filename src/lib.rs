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
pub mod telemetry;
pub mod ui;
pub mod worker;

use thiserror::Error;

// -- Debug capture --

/// Raw request/response snapshot for the on-demand debug view. Populated on
/// every LLM call regardless of success or failure, so both a wrong-looking
/// answer and an outright error expose the exact bytes exchanged with the
/// server, plus when it happened and how long it took. Surfaced via the
/// overlay's "copy debug" button; not logged by default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DebugCapture {
    /// Target URL the request was sent to.
    pub endpoint: Option<String>,
    /// Pretty-printed request body JSON. Inline image data URIs are elided to
    /// keep the snapshot readable.
    pub request: Option<String>,
    /// HTTP status code of the response, when one arrived.
    pub status: Option<u16>,
    /// Raw response body: the full JSON for a non-streaming reply, the
    /// concatenated SSE stream for a streaming reply, or the server's error
    /// body for a non-2xx rejection.
    pub response_raw: Option<String>,
    /// Wall-clock time (UTC, e.g. `2026-06-25T01:02:03.123Z`) the request began.
    pub timestamp: Option<String>,
    /// Round-trip duration in milliseconds from request start to the final
    /// response, error, or stream close.
    pub elapsed_ms: Option<u128>,
    /// Detailed failure description (the underlying transport/API error or the
    /// reason a stream ended early). Present only when something went wrong;
    /// the short user-facing message is shown separately in the overlay.
    pub error: Option<String>,
    /// Token usage reported by the server, when present. Not every server
    /// includes this (streaming responses in particular, since we do not
    /// request `stream_options.include_usage`), so it is `None` more often
    /// than not.
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
}

impl DebugCapture {
    /// Format the capture as a single human-readable block for the clipboard.
    pub fn to_clipboard_text(&self) -> String {
        let endpoint = self.endpoint.as_deref().unwrap_or("(unknown endpoint)");
        let status = self
            .status
            .map_or_else(|| "(no response)".to_string(), |s| s.to_string());
        let timestamp = self.timestamp.as_deref().unwrap_or("(unknown)");
        let elapsed = self
            .elapsed_ms
            .map_or_else(|| "(unknown)".to_string(), |ms| format!("{ms} ms"));
        let request = self.request.as_deref().unwrap_or("(not captured)");
        let response = self.response_raw.as_deref().unwrap_or("(not captured)");
        let mut out = format!(
            "=== clip-llm debug ===\n\
             time:        {timestamp}\n\
             elapsed:     {elapsed}\n\
             endpoint:    POST {endpoint}\n\
             HTTP status: {status}\n"
        );
        if let (Some(p), Some(c), Some(t)) =
            (self.prompt_tokens, self.completion_tokens, self.total_tokens)
        {
            out.push_str(&format!(
                "tokens:      prompt={p} completion={c} total={t}\n"
            ));
        }
        if let Some(err) = &self.error {
            out.push_str(&format!("\n--- ERROR ---\n{err}\n"));
        }
        out.push_str(&format!("\n--- REQUEST ---\n{request}\n"));
        out.push_str(&format!("\n--- RESPONSE ---\n{response}\n"));
        out
    }
}

/// Format a `SystemTime` as an ISO-8601 UTC string (`YYYY-MM-DDThh:mm:ss.mmmZ`)
/// without pulling in a calendar crate. Uses Howard Hinnant's `civil_from_days`
/// algorithm. `chrono` is only a `diagnostics`-feature dependency, so the
/// always-on debug capture cannot rely on it.
pub fn format_utc(t: std::time::SystemTime) -> String {
    let dur = t
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil_from_days: days since 1970-01-01 -> (year, month, day).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z")
}

#[cfg(test)]
mod debug_capture_tests {
    use super::{format_utc, DebugCapture};
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn to_clipboard_text_includes_all_sections() {
        let cap = DebugCapture {
            endpoint: Some("http://x/v1/chat/completions".into()),
            request: Some("{\"model\":\"m\"}".into()),
            status: Some(200),
            response_raw: Some("{\"choices\":[]}".into()),
            timestamp: Some("2026-06-25T01:02:03.123Z".into()),
            elapsed_ms: Some(1234),
            error: None,
            ..Default::default()
        };
        let text = cap.to_clipboard_text();
        assert!(text.contains("time:        2026-06-25T01:02:03.123Z"));
        assert!(text.contains("elapsed:     1234 ms"));
        assert!(text.contains("POST http://x/v1/chat/completions"));
        assert!(text.contains("HTTP status: 200"));
        assert!(text.contains("--- REQUEST ---"));
        assert!(text.contains("\"model\":\"m\""));
        assert!(text.contains("--- RESPONSE ---"));
        assert!(text.contains("\"choices\":[]"));
        // No error section when the call succeeded.
        assert!(!text.contains("--- ERROR ---"));
        // No token-usage line when usage was not captured.
        assert!(!text.contains("tokens:"));
    }

    #[test]
    fn to_clipboard_text_shows_token_usage_when_present() {
        let cap = DebugCapture {
            prompt_tokens: Some(120),
            completion_tokens: Some(45),
            total_tokens: Some(165),
            ..Default::default()
        };
        let text = cap.to_clipboard_text();
        assert!(text.contains("tokens:      prompt=120 completion=45 total=165"));
    }

    #[test]
    fn to_clipboard_text_shows_error_section() {
        let cap = DebugCapture {
            error: Some("connect timeout".into()),
            ..Default::default()
        };
        let text = cap.to_clipboard_text();
        assert!(text.contains("--- ERROR ---"));
        assert!(text.contains("connect timeout"));
    }

    #[test]
    fn to_clipboard_text_marks_missing_fields() {
        let text = DebugCapture::default().to_clipboard_text();
        assert!(text.contains("(no response)"));
        assert!(text.contains("(not captured)"));
        assert!(text.contains("time:        (unknown)"));
    }

    #[test]
    fn format_utc_epoch_and_known_instant() {
        assert_eq!(format_utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // 1_700_000_000s -> 2023-11-14T22:13:20 UTC, plus 123ms.
        let t = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        assert_eq!(format_utc(t), "2023-11-14T22:13:20.123Z");
    }
}

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

    /// Next mode in `ALL` (display order), wrapping around, skipping any mode
    /// not present in `targets`. Returns `self` unchanged when no other mode is
    /// available (empty `targets`, or `targets` holds only `self`). Used to
    /// cycle the mode while the hotkey modifiers are held.
    pub fn next_available(self, targets: &[ProcessMode]) -> ProcessMode {
        if targets.is_empty() {
            return self;
        }
        let all = Self::ALL;
        let start = all.iter().position(|&m| m == self).unwrap_or(0);
        // Walk forward from the mode after `self`, wrapping; first match wins.
        // The final offset returns to `self`, so a single-mode target yields self.
        for offset in 1..=all.len() {
            let candidate = all[(start + offset) % all.len()];
            if targets.contains(&candidate) {
                return candidate;
            }
        }
        self
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
        let mode_prompt = match self {
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
        };
        // Prepend the shared preamble (`[prompt].preamble`) that applies to every
        // mode, if configured. Empty/absent leaves the mode prompt unchanged.
        match config.prompt_preamble() {
            Some(preamble) => format!(
                "{}\n\n{mode_prompt}",
                crate::config::substitute(preamble, primary, secondary)
            ),
            None => mode_prompt,
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

    #[error(
        "required setting not configured: {0} \
         (set it in config.toml under [api], or the matching CLIP_LLM_* environment variable)"
    )]
    MissingConfig(&'static str),

    #[error("no response headers within {0}s")]
    InitialResponseTimeout(u64),

    #[error("empty response from model")]
    EmptyResponse,

    /// The server returned 2xx but the body is not a chat completion (e.g. a
    /// proxy served an HTML error page with status 200, or the body was cut).
    #[error("malformed response body: {0}")]
    MalformedResponse(String),

    #[error("no usable content: image-only clipboard but model lacks vision support")]
    NoUsableContent,

    #[error("response truncated: generation hit the max_tokens limit")]
    Truncated,

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

    /// A copy landed on the clipboard (change counter bumped) but carried
    /// no usable text or image — e.g. a whitespace-only selection.
    #[error("copy delivered no usable content")]
    EmptyCopy,

    #[error("capture cancelled")]
    Cancelled,

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_available_wraps_through_all() {
        let all = ProcessMode::ALL;
        assert_eq!(ProcessMode::Translate.next_available(all), ProcessMode::Rephrase);
        assert_eq!(ProcessMode::Rephrase.next_available(all), ProcessMode::Summarize);
        // Wraps back to the first mode.
        assert_eq!(ProcessMode::Summarize.next_available(all), ProcessMode::Translate);
    }

    #[test]
    fn next_available_skips_unavailable() {
        // Rephrase is not a target — it is skipped.
        let targets = &[ProcessMode::Translate, ProcessMode::Summarize];
        assert_eq!(ProcessMode::Translate.next_available(targets), ProcessMode::Summarize);
        assert_eq!(ProcessMode::Summarize.next_available(targets), ProcessMode::Translate);
    }

    #[test]
    fn next_available_single_target_stays_put() {
        // Image-only content restricts cycling to Summarize alone.
        let targets = &[ProcessMode::Summarize];
        assert_eq!(ProcessMode::Summarize.next_available(targets), ProcessMode::Summarize);
        // Even starting from a non-target mode resolves to the only target.
        assert_eq!(ProcessMode::Translate.next_available(targets), ProcessMode::Summarize);
    }

    #[test]
    fn next_available_empty_targets_returns_self() {
        assert_eq!(ProcessMode::Translate.next_available(&[]), ProcessMode::Translate);
    }
}

