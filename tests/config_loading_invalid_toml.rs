//! End-to-end check of the startup config-loading path when the external file
//! at `CLIP_LLM_CONFIG` is malformed TOML: the process must fall back to
//! built-in defaults (never panic) and report the failure via `LoadOutcome`.
//!
//! This runs as its own test binary, so the process-global config `OnceLock`
//! starts uninitialized and `init()` is the first reader.
//!
//! IMPORTANT: keep exactly one `#[test]` in this file, for the same reason as
//! `tests/config_loading.rs` — `init()` writes the `OnceLock` once.

use clip_llm::{config, ProcessMode, RephraseParams};

#[test]
fn startup_falls_back_to_defaults_on_invalid_toml() {
    let path =
        std::env::temp_dir().join(format!("clip_llm_cfg_invalid_{}.toml", std::process::id()));
    // Malformed on line 2: an unquoted, non-boolean/number/date value.
    std::fs::write(&path, "[api]\nendpoint = not-a-string\n").unwrap();

    // SAFETY: this test binary is single-threaded here (no worker/UI threads have
    // been spawned), so no other thread can concurrently access the process
    // environment — set_var/remove_var are sound.
    unsafe {
        std::env::set_var("CLIP_LLM_CONFIG", &path);
    }

    config::init();

    // The load outcome reflects the failure, carrying the offending path
    // alongside a generic (file-content-free) reason.
    assert_eq!(
        config::load_outcome(),
        config::LoadOutcome::Failed { path: path.clone(), reason: "invalid TOML" }
    );

    // Built-in defaults remain active despite the malformed file.
    let translate = ProcessMode::Translate.system_prompt(RephraseParams::default());
    assert!(translate.contains("translator"));

    let _ = std::fs::remove_file(&path);
    // SAFETY: same invariant as above — still single-threaded, no concurrent
    // environment access.
    unsafe {
        std::env::remove_var("CLIP_LLM_CONFIG");
    }
}
