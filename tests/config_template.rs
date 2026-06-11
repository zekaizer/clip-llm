//! End-to-end check of `ensure_config_file()`: writes the commented starter
//! template when no config exists, and never touches an existing file.
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

    // Fully commented: every non-blank line is a comment, so the built-in
    // defaults (notably the rich prompts) stay active.
    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(!contents.is_empty());
    assert!(contents
        .lines()
        .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#')));

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
