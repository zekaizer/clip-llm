use std::sync::LazyLock;

use regex::Regex;

static THINK_BLOCK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<think>.*?</think>").unwrap());

/// Strip `<think>...</think>` blocks from LLM response and trim whitespace.
pub fn strip_think_blocks(text: &str) -> String {
    THINK_BLOCK_RE.replace_all(text, "").trim().to_string()
}
