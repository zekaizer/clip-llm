//! End-to-end check of the tray "Reload Config" path: after `init()`, editing
//! the file and calling `reload()` swaps the new values in, while a broken
//! edit is rejected and leaves the active config untouched.
//!
//! Own test binary (process-global config); keep exactly one `#[test]` here.

use clip_llm::config;

#[test]
fn reload_swaps_in_new_values_and_keeps_previous_on_failure() {
    let path = std::env::temp_dir().join(format!("clip_llm_reload_{}.toml", std::process::id()));
    std::fs::write(&path, "[languages]\nprimary = \"Japanese\"\n").unwrap();
    // SAFETY: single-threaded at this point — no other thread touches the environment.
    unsafe {
        std::env::set_var("CLIP_LLM_CONFIG", &path);
    }

    config::init();
    assert_eq!(config::get().primary_lang(), "Japanese");
    let before = config::get();

    std::fs::write(&path, "[languages]\nprimary = \"German\"\n[ui]\ntabs = [\"summarize\"]\n").unwrap();
    assert_eq!(config::reload(), Ok(path.clone()));
    let after = config::get();
    assert_eq!(after.primary_lang(), "German");
    assert_eq!(before.primary_lang(), "Japanese", "an older snapshot stays valid");
    assert_eq!(before.restart_required_changes(&after), vec!["[ui].tabs"]);
    assert_eq!(config::load_outcome(), config::LoadOutcome::Loaded(path.clone()));

    // A broken file is rejected: the German config stays active.
    std::fs::write(&path, "[languages\nprimary = 1").unwrap();
    assert_eq!(config::reload(), Err("invalid TOML"));
    assert_eq!(config::get().primary_lang(), "German");
    assert_eq!(config::load_outcome(), config::LoadOutcome::Loaded(path.clone()));

    let _ = std::fs::remove_file(&path);
    // SAFETY: same invariant as above.
    unsafe {
        std::env::remove_var("CLIP_LLM_CONFIG");
    }
}
