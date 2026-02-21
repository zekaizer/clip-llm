use std::sync::LazyLock;

use regex::Regex;

static THINK_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<think>.*?</think>").unwrap());

/// Strip `<think>...</think>` blocks from LLM response.
/// Only trims leading newlines (from think-block removal) and trailing whitespace,
/// preserving leading indentation on the first content line.
pub fn strip_think_blocks(text: &str) -> String {
    THINK_BLOCK_RE
        .replace_all(text, "")
        .trim_end()
        .trim_start_matches(['\n', '\r'])
        .to_string()
}

const OPEN_TAG: &str = "<think>";
const CLOSE_TAG: &str = "</think>";

/// Incremental filter that strips the first `<think>...</think>` block from a
/// token stream. Feed tokens one at a time via [`feed`]; only visible text is
/// returned. After the closing tag, all subsequent tokens pass through unchanged.
pub struct ThinkBlockFilter {
    state: ThinkState,
    pending: String,
    trim_leading_newlines: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkState {
    /// Haven't seen `<think>` yet — buffering potential prefix.
    BeforeThink,
    /// Inside `<think>…</think>` — suppressing output.
    InsideThink,
    /// Past the think block (or none existed) — pass-through.
    PassThrough,
}

impl Default for ThinkBlockFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl ThinkBlockFilter {
    pub fn new() -> Self {
        Self {
            state: ThinkState::BeforeThink,
            pending: String::new(),
            trim_leading_newlines: false,
        }
    }

    /// Feed a streaming token; returns the text to display (may be empty).
    pub fn feed(&mut self, token: &str) -> String {
        match self.state {
            ThinkState::BeforeThink => self.feed_before(token),
            ThinkState::InsideThink => self.feed_inside(token),
            ThinkState::PassThrough => self.feed_passthrough(token),
        }
    }

    fn feed_before(&mut self, token: &str) -> String {
        self.pending.push_str(token);

        if self.pending.starts_with(OPEN_TAG) {
            // Full open tag matched (possibly with trailing chars).
            let remainder = self.pending[OPEN_TAG.len()..].to_string();
            self.pending.clear();
            self.state = ThinkState::InsideThink;
            self.feed_inside(&remainder)
        } else if OPEN_TAG.starts_with(self.pending.as_str()) {
            // Pending is a proper prefix of "<think>" — keep buffering.
            String::new()
        } else {
            // Not a think tag — flush everything accumulated.
            let flushed = std::mem::take(&mut self.pending);
            self.state = ThinkState::PassThrough;
            flushed
        }
    }

    fn feed_inside(&mut self, token: &str) -> String {
        self.pending.push_str(token);

        if let Some(pos) = self.pending.find(CLOSE_TAG) {
            let after = &self.pending[pos + CLOSE_TAG.len()..];
            let trimmed = after.trim_start_matches(['\n', '\r']).to_string();
            self.trim_leading_newlines = trimmed.is_empty();
            self.pending.clear();
            self.state = ThinkState::PassThrough;
            trimmed
        } else {
            // Retain last (CLOSE_TAG.len() - 1) bytes so a split close tag
            // spanning two chunks is still detected on the next feed.
            let keep = CLOSE_TAG.len() - 1;
            if self.pending.len() > keep {
                let mut start = self.pending.len() - keep;
                while !self.pending.is_char_boundary(start) {
                    start -= 1;
                }
                self.pending = self.pending[start..].to_string();
            }
            String::new()
        }
    }

    fn feed_passthrough(&mut self, token: &str) -> String {
        if self.trim_leading_newlines {
            let trimmed = token.trim_start_matches(['\n', '\r']);
            if trimmed.is_empty() {
                return String::new();
            }
            self.trim_leading_newlines = false;
            trimmed.to_string()
        } else {
            token.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_think_block() {
        assert_eq!(strip_think_blocks("hello world"), "hello world");
    }

    #[test]
    fn single_think_block() {
        let input = "<think>reasoning here</think>answer";
        assert_eq!(strip_think_blocks(input), "answer");
    }

    #[test]
    fn multiline_think_block() {
        let input = "<think>\nstep 1\nstep 2\n</think>\nfinal answer";
        assert_eq!(strip_think_blocks(input), "final answer");
    }

    #[test]
    fn multiple_think_blocks() {
        let input = "<think>a</think>hello <think>b</think>world";
        assert_eq!(strip_think_blocks(input), "hello world");
    }

    #[test]
    fn think_only_returns_empty() {
        let input = "<think>only reasoning</think>";
        assert_eq!(strip_think_blocks(input), "");
    }

    #[test]
    fn preserves_text_without_tags() {
        let input = "no tags here, just <angle> brackets";
        assert_eq!(strip_think_blocks(input), "no tags here, just <angle> brackets");
    }

    #[test]
    fn trims_trailing_whitespace_only() {
        let input = "  <think>x</think>  result  ";
        assert_eq!(strip_think_blocks(input), "    result");
    }

    #[test]
    fn preserves_leading_indentation() {
        let input = "<think>x</think>\n    indented\n        more";
        assert_eq!(strip_think_blocks(input), "    indented\n        more");
    }

    // --- ThinkBlockFilter tests ---

    #[test]
    fn filter_no_think_passthrough() {
        let mut f = ThinkBlockFilter::new();
        assert_eq!(f.feed("hello"), "hello");
        assert_eq!(f.feed(" world"), " world");
    }

    #[test]
    fn filter_think_suppressed() {
        let mut f = ThinkBlockFilter::new();
        assert_eq!(f.feed("<think>"), "");
        assert_eq!(f.feed("reasoning"), "");
        assert_eq!(f.feed("</think>"), "");
        assert_eq!(f.feed("answer"), "answer");
    }

    #[test]
    fn filter_split_across_tokens() {
        let mut f = ThinkBlockFilter::new();
        assert_eq!(f.feed("<thi"), "");
        assert_eq!(f.feed("nk>"), "");
        assert_eq!(f.feed("some reasoning text"), "");
        assert_eq!(f.feed("</thi"), "");
        assert_eq!(f.feed("nk>"), "");
        assert_eq!(f.feed("answer"), "answer");
    }

    #[test]
    fn filter_not_think_flushes_pending() {
        let mut f = ThinkBlockFilter::new();
        assert_eq!(f.feed("<thi"), "");
        assert_eq!(f.feed("s is not"), "<this is not");
        assert_eq!(f.feed(" a tag"), " a tag");
    }

    #[test]
    fn filter_newlines_trimmed_after_think() {
        let mut f = ThinkBlockFilter::new();
        assert_eq!(f.feed("<think>x</think>"), "");
        assert_eq!(f.feed("\n"), "");
        assert_eq!(f.feed("\n"), "");
        assert_eq!(f.feed("answer"), "answer");
    }

    #[test]
    fn filter_empty_input() {
        let mut f = ThinkBlockFilter::new();
        assert_eq!(f.feed(""), "");
    }
}
