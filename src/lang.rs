//! Deterministic input-language cues for the Translate mode: whether the
//! prose (sentences, comments, string literals; never identifiers or URLs)
//! is Korean, and whether the input is a dictionary lookup (a word or a
//! short term). Decided in code so the prompt can state the direction
//! instead of asking the model to infer it.

/// The prose of `text` is Korean - the source-language decision for the
/// translation direction. Non-prose (code, URLs, numbers) is ignored; an
/// input without any prose is not Korean.
pub fn prose_is_korean(text: &str) -> bool {
    let mut hangul = 0usize;
    let mut latin = 0usize;
    let mut ko_markers = 0usize;
    let mut en_function_words = 0usize;
    for line in text.lines() {
        // A statement line (SQL, C-likes): its English-looking keywords are
        // not prose; only a Korean comment on it is.
        let code_line = line.trim_end().ends_with([';', '{', '}']);
        for token in line.split_whitespace() {
            let core = token.trim_matches(|c: char| PUNCTUATION.contains(c));
            if core.is_empty() {
                continue;
            }
            let hangul_here = core.chars().filter(|&c| is_hangul(c)).count();
            if hangul_here > 0 {
                hangul += hangul_here;
                if has_korean_marker(core) {
                    ko_markers += 1;
                }
                continue;
            }
            if code_line || is_code_like(core) || is_all_caps(core) {
                continue;
            }
            if core.len() >= 2 && core.chars().all(|c| c.is_ascii_alphabetic()) {
                latin += core.len();
                if EN_FUNCTION_WORDS.contains(&core.to_ascii_lowercase().as_str()) {
                    en_function_words += 1;
                }
            }
        }
    }
    // Grammar first: particles and endings say which language the sentence is
    // written in even when English terms outnumber the Korean words.
    if ko_markers != en_function_words {
        return ko_markers > en_function_words;
    }
    // Then the script ratio; one Hangul syllable carries about as much text
    // as 2.5 Latin letters.
    let hangul_weight = hangul as f64 * HANGUL_WEIGHT;
    let total = hangul_weight + latin as f64;
    total > 0.0 && hangul_weight / total >= 0.5
}

/// Text weight of one Hangul syllable relative to one Latin letter.
const HANGUL_WEIGHT: f64 = 2.5;

/// Punctuation stripped from token ends before classification.
const PUNCTUATION: &str = ",.;:!?'\"()[]{}<>\u{2014}-*`\u{201c}\u{201d}\u{2018}\u{2019}";

/// English words that only appear in English sentences (never as terms
/// inside Korean prose).
const EN_FUNCTION_WORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "with", "is", "are", "was", "were", "be",
    "been", "this", "that", "it", "we", "you", "they", "not", "by", "from", "as", "at", "if", "then",
    "when", "which", "who", "will", "would", "can", "could", "should", "has", "have", "had", "do", "does",
    "did", "but", "so", "than", "into", "please", "after", "before",
];

/// Korean particles and sentence endings, matched at the end of a token.
const KO_MARKERS: &[&str] = &[
    "은", "는", "이", "가", "을", "를", "의", "에", "에서", "로", "으로", "와", "과", "도", "만", "까지", "부터",
    "다", "요", "니다", "함", "음", "됨", "임",
];

fn is_hangul(c: char) -> bool {
    matches!(c, '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3131}'..='\u{318E}')
}

fn has_korean_marker(core: &str) -> bool {
    KO_MARKERS.iter().any(|m| core.ends_with(m))
}

/// Keywords and acronyms (SELECT, API, JSON): terms, not prose words.
fn is_all_caps(core: &str) -> bool {
    core.len() >= 2 && core.chars().all(|c| c.is_ascii_uppercase())
}

/// Identifiers, paths, URLs, operators: anything with symbol characters inside.
fn is_code_like(core: &str) -> bool {
    core.chars().any(|c| "_.:/\\=(){}[]<>@#$%^&*|~+".contains(c))
}

/// `text` is a single word to look up rather than text to translate.
pub fn is_lookup(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() || text.contains('\n') || text.chars().count() > LOOKUP_MAX_CHARS {
        return false;
    }
    // Something to define: letters or syllables, not emoji or numbers.
    if !text.chars().any(char::is_alphabetic) {
        return false;
    }
    // One word. Two words are already a phrase ("번역 완료" is a status, not
    // a term) and get translated.
    let mut tokens = text.split_whitespace();
    let (Some(word), None) = (tokens.next(), tokens.next()) else { return false };
    if is_code_like(word.trim_matches(|c: char| PUNCTUATION.contains(c))) {
        return false;
    }
    // A sentence, however short, is translated: terminal punctuation, Latin or CJK.
    !word.ends_with(['.', '!', '?', '\u{3002}', '\u{ff01}', '\u{ff1f}'])
}

const LOOKUP_MAX_CHARS: usize = 40;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Case {
        id: String,
        expect: String,
        lookup: bool,
        text: String,
    }

    /// Shared with the harness self-tests (scripts/test_verify_prompts.py).
    fn cases() -> Vec<Case> {
        serde_json::from_str(include_str!("../scripts/lang_direction_vectors.json")).expect("valid fixture")
    }

    #[test]
    fn direction_matches_the_vectors() {
        let mut wrong = Vec::new();
        for c in cases() {
            if c.expect == "either" {
                continue;
            }
            let got = if prose_is_korean(&c.text) { "ko" } else { "en" };
            if got != c.expect {
                wrong.push(format!("{}: expected {} got {}: {:?}", c.id, c.expect, got, c.text));
            }
        }
        assert!(wrong.is_empty(), "{} wrong:\n{}", wrong.len(), wrong.join("\n"));
    }

    #[test]
    fn lookup_matches_the_vectors() {
        let mut wrong = Vec::new();
        for c in cases() {
            if is_lookup(&c.text) != c.lookup {
                wrong.push(format!("{}: expected lookup={} : {:?}", c.id, c.lookup, c.text));
            }
        }
        assert!(wrong.is_empty(), "{} wrong:\n{}", wrong.len(), wrong.join("\n"));
    }
}
