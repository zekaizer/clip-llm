//! End-to-end check of `ensure_config_file()`: writes the starter template when
//! no config exists, and never touches an existing file. The template comments
//! out every optional key (defaults stay active) but emits the three REQUIRED
//! keys (api.endpoint/model/api_key — no defaults) uncommented and empty.
//!
//! Runs as its own test binary so the `CLIP_LLM_CONFIG` environment override
//! cannot interfere with other tests (same isolation rationale as
//! `config_loading.rs` — keep exactly one `#[test]` per env-mutating file).

use clip_llm::config;

#[test]
fn ensure_config_file_creates_commented_template_once() {
    let path = std::env::temp_dir().join(format!("clip_llm_tpl_{}.toml", std::process::id()));
    let _ = std::fs::remove_file(&path);

    // SAFETY: this test binary is single-threaded here (no worker/UI threads
    // have been spawned), so no other thread can concurrently access the
    // process environment — set_var/remove_var are sound.
    unsafe {
        std::env::set_var("CLIP_LLM_CONFIG", &path);
    }

    // Creates the starter template at the candidate path.
    let created = config::ensure_config_file().expect("template should be created");
    assert_eq!(created, path);

    // Optional keys stay commented (built-in defaults remain active); the only
    // uncommented lines are the [api] header and the three REQUIRED keys, which
    // are emitted empty so the user must fill them in.
    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(!contents.is_empty());
    let uncommented: Vec<&str> = contents
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert_eq!(
        uncommented.len(),
        4,
        "only [api] + the 3 required keys should be uncommented, got: {uncommented:?}"
    );
    assert!(uncommented.contains(&"[api]"));
    assert!(uncommented.iter().any(|l| l.starts_with("endpoint = \"\"")));
    assert!(uncommented.iter().any(|l| l.starts_with("model") && l.contains("= \"\"")));
    assert!(uncommented.iter().any(|l| l.starts_with("api_key") && l.contains("= \"\"")));
    // Every section is present (sampling a few).
    for section in ["[api]", "[generation]", "[rephrase.style]", "[summarize]"] {
        assert!(contents.contains(section), "missing section {section}");
    }

    // Idempotent: a second call returns the existing file untouched.
    std::fs::write(&path, "# user edited\n").unwrap();
    let again = config::ensure_config_file().expect("existing file should be kept");
    assert_eq!(again, path);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "# user edited\n");

    let _ = std::fs::remove_file(&path);
    // SAFETY: same invariant as above — still single-threaded.
    unsafe {
        std::env::remove_var("CLIP_LLM_CONFIG");
    }
}
