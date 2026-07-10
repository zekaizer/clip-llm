//! End-to-end check that a `[generation]` field set to a meaningless `0`
//! (which would make `reqwest` apply an instant timeout, or cap the response
//! at zero tokens) is sanitized away at load time, so the built-in default
//! applies instead of being passed through to the client.
//!
//! This runs as its own test binary, so the process-global config `OnceLock`
//! starts uninitialized and `init()` is the first reader.
//!
//! IMPORTANT: keep exactly one `#[test]` in this file, for the same reason as
//! `tests/config_loading.rs` — `init()` writes the `OnceLock` once.

use clip_llm::config;

#[test]
fn startup_clears_zero_generation_fields() {
    let path = std::env::temp_dir()
        .join(format!("clip_llm_cfg_gen_zero_{}.toml", std::process::id()));
    std::fs::write(
        &path,
        "[generation]\n\
         temperature = 0.0\n\
         max_tokens = 0\n\
         token_budget = 0\n\
         request_timeout_secs = 0\n\
         initial_response_timeout_secs = 0\n",
    )
    .unwrap();

    // SAFETY: this test binary is single-threaded here (no worker/UI threads have
    // been spawned), so no other thread can concurrently access the process
    // environment — set_var/remove_var are sound.
    unsafe {
        std::env::set_var("CLIP_LLM_CONFIG", &path);
    }

    config::init();

    let cfg = config::get();
    // Zero timeouts/token caps are meaningless, so they must be dropped back
    // to "unset" (the built-in default applies downstream).
    assert_eq!(cfg.generation_max_tokens(), None);
    assert_eq!(cfg.generation_token_budget(), None);
    assert_eq!(cfg.generation_request_timeout_secs(), None);
    assert_eq!(cfg.generation_initial_response_timeout_secs(), None);
    // temperature = 0.0 is a legitimate deterministic-sampling value and must
    // survive untouched.
    assert_eq!(cfg.generation_temperature(), Some(0.0));

    let _ = std::fs::remove_file(&path);
    // SAFETY: same invariant as above — still single-threaded, no concurrent
    // environment access.
    unsafe {
        std::env::remove_var("CLIP_LLM_CONFIG");
    }
}
