use std::sync::LazyLock;

use regex::Regex;

static THINK_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<think>.*?</think>").unwrap());

/// Strip `<think>...</think>` blocks from LLM response and trim whitespace.
pub fn strip_think_blocks(text: &str) -> String {
    THINK_BLOCK_RE.replace_all(text, "").trim().to_string()
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
    fn trims_surrounding_whitespace() {
        let input = "  <think>x</think>  result  ";
        assert_eq!(strip_think_blocks(input), "result");
    }
}
