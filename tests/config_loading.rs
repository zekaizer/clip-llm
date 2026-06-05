//! End-to-end check of the startup config-loading path: an external TOML file
//! pointed at by `CLIP_LLM_CONFIG` is parsed and applied, with untouched modes
//! falling back to the built-in defaults.
//!
//! This runs as its own test binary, so the process-global config `OnceLock`
//! starts uninitialized and `init_prompt_config()` is the first reader.

use clip_llm::{config, ProcessMode, RephraseParams};

#[test]
fn startup_loads_override_from_env_path() {
    let path = std::env::temp_dir().join(format!("clip_llm_cfg_{}.toml", std::process::id()));
    std::fs::write(
        &path,
        "[translate]\nprompt = \"OVERRIDE {primary_lang}->{secondary_lang}\"\n",
    )
    .unwrap();

    // SAFETY: single-threaded test setup; no other thread reads the environment
    // before init_prompt_config() runs below.
    unsafe {
        std::env::set_var("CLIP_LLM_CONFIG", &path);
    }

    config::init_prompt_config();

    // Overridden mode reflects the file, with placeholders substituted.
    let translate = ProcessMode::Translate.system_prompt(RephraseParams::default(), false);
    assert_eq!(translate, "OVERRIDE Korean->English");

    // Untouched modes keep the built-in defaults.
    let summarize = ProcessMode::Summarize.system_prompt(RephraseParams::default(), false);
    assert!(summarize.contains("text summarizer for software engineering content"));

    let _ = std::fs::remove_file(&path);
    unsafe {
        std::env::remove_var("CLIP_LLM_CONFIG");
    }
}
