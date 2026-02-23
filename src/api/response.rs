use std::sync::LazyLock;

use regex::Regex;

static THINK_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<think>.*?</think>").unwrap());

static THINK_CAPTURE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<think>(.*?)</think>").unwrap());

/// Extract the content of the first `<think>...</think>` block (tags removed).
/// Returns `None` if no complete think block exists.
pub fn extract_first_think_content(text: &str) -> Option<String> {
    THINK_CAPTURE_RE
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

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
    /// Accumulated content of the first think block (populated when InsideThink).
    think_content: String,
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
            think_content: String::new(),
        }
    }

    /// Returns `true` while inside a `<think>…</think>` block.
    pub fn is_thinking(&self) -> bool {
        matches!(self.state, ThinkState::InsideThink)
    }

    /// Take the accumulated think-block content, leaving it empty.
    /// Only meaningful after the close tag has been processed.
    pub fn take_think_content(&mut self) -> String {
        std::mem::take(&mut self.think_content)
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
            // Save the think content before the close tag.
            self.think_content.push_str(&self.pending[..pos]);
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
                // Accumulate the discarded prefix into think_content.
                self.think_content.push_str(&self.pending[..start]);
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

    // Verify that the close tag is detected regardless of which byte it is split at.
    // The `keep = CLOSE_TAG.len() - 1` tail-preservation logic must handle all splits.
    #[test]
    fn filter_close_tag_split_at_each_byte() {
        let close = "</think>";
        for split in 1..close.len() {
            let (first, second) = close.split_at(split);
            let mut f = ThinkBlockFilter::new();
            assert_eq!(f.feed("<think>reasoning"), "", "split at {split}: open");
            assert_eq!(f.feed(first), "", "split at {split}: first half");
            assert_eq!(f.feed(second), "", "split at {split}: second half");
            assert_eq!(f.feed("answer"), "answer", "split at {split}: after");
        }
    }

    // Content immediately following the close tag (no leading newline) must be
    // returned immediately and must NOT set trim_leading_newlines.
    #[test]
    fn filter_content_immediately_after_close_tag() {
        let mut f = ThinkBlockFilter::new();
        assert_eq!(f.feed("<think>x</think>answer"), "answer");
        assert_eq!(f.feed(" more"), " more");
    }

    // After the first think block the filter enters PassThrough.
    // Any subsequent <think> tokens must be passed through verbatim.
    #[test]
    fn filter_second_think_block_passes_through() {
        let mut f = ThinkBlockFilter::new();
        assert_eq!(f.feed("<think>first</think>"), "");
        // PassThrough: second block is NOT filtered.
        assert_eq!(f.feed("<think>second</think>"), "<think>second</think>");
        assert_eq!(f.feed(" rest"), " rest");
    }

    // Korean 3-byte chars near the tail preservation boundary must not cause a
    // char-boundary panic. The char_boundary adjustment in feed_inside is exercised.
    #[test]
    fn filter_utf8_boundary_at_close_tag_tail() {
        let mut f = ThinkBlockFilter::new();
        // "가나다" = 9 bytes; keep = CLOSE_TAG.len()-1 = 7.
        // After feed("<think>가나다"), pending inside = "가나다" (9 bytes, no truncation yet).
        // After feed("</thi"), pending = "가나다</thi" (15 bytes) → truncated to last 7 bytes.
        // Byte 15-7=8 lands inside "다" (bytes 6-8), so char_boundary loop backs up to byte 6.
        assert_eq!(f.feed("<think>가나다"), "");
        assert_eq!(f.feed("</thi"), ""); // must not panic
        assert_eq!(f.feed("nk>"), "");
        assert_eq!(f.feed("answer"), "answer");
    }

    // Leading newlines after close tag are trimmed; the first non-newline token
    // stops trimming and is passed through intact, with no further trimming.
    #[test]
    fn filter_newline_then_non_newline_after_close_tag() {
        let mut f = ThinkBlockFilter::new();
        assert_eq!(f.feed("<think>x</think>"), "");
        assert_eq!(f.feed("\n"), "");
        assert_eq!(f.feed("\n"), "");
        assert_eq!(f.feed("answer\n"), "answer\n"); // trim_leading_newlines reset after first non-newline
        assert_eq!(f.feed("\n"), "\n"); // subsequent newlines pass through
    }

    // --- strip_think_blocks edge cases ---

    // An unclosed <think> tag is not matched by the regex and must be preserved.
    #[test]
    fn unclosed_think_tag_preserved() {
        let input = "<think>no closing tag";
        assert_eq!(strip_think_blocks(input), "<think>no closing tag");
    }

    // Non-greedy `.*?` correctly handles nested markup inside the think block.
    #[test]
    fn nested_markup_inside_think_block() {
        let input = "<think><b>bold reasoning</b></think>answer";
        assert_eq!(strip_think_blocks(input), "answer");
    }
}
