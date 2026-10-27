//! A per-mode `thinking` override in the config file changes the mode's
//! effective default but not its built-in one (the Settings tooltip shows the
//! built-in value next to the "Default" pill).
//!
//! Own test binary (process-global config); keep exactly one `#[test]` here.

use clip_llm::{config, ProcessMode, ThinkingMode};

#[test]
fn thinking_override_leaves_the_builtin_default_alone() {
    let path = std::env::temp_dir().join(format!("clip_llm_thinking_{}.toml", std::process::id()));
    std::fs::write(&path, "[translate]\nthinking = \"think\"\n").unwrap();
    // SAFETY: single-threaded at this point — no other thread touches the environment.
    unsafe {
        std::env::set_var("CLIP_LLM_CONFIG", &path);
    }

    config::init();
    assert_eq!(ProcessMode::Translate.default_thinking(), ThinkingMode::Think);
    assert_eq!(ProcessMode::Translate.builtin_thinking(), ThinkingMode::NoThink);

    let _ = std::fs::remove_file(&path);
    // SAFETY: same invariant as above.
    unsafe {
        std::env::remove_var("CLIP_LLM_CONFIG");
    }
}
