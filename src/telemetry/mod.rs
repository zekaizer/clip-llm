//! Logging / tracing setup.
//!
//! stderr logging (the `fmt` layer) is always on. When `[logging].url` is set
//! in the config, a second [`victoria::VictoriaLayer`] is added that ships
//! structured events to a VictoriaLogs instance (opt-in; see the module docs).
//!
//! Each sink carries its own per-layer filter so they are independent: stderr
//! keeps the usual `clip_llm=info` (or `RUST_LOG`) level, while the remote sink
//! ships `clip_llm` events at `[logging].level` (default `info`). The remote
//! filter is scoped to the `clip_llm` target so dependency logs (reqwest/hyper)
//! are never shipped — which also prevents the shipper's own HTTP from
//! recursing back into the sink.

mod victoria;

use std::time::Duration;

use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

/// Initialize the global tracing subscriber. Reads `[logging]` from the loaded
/// config, so `crate::config::init()` must run first.
pub fn init() {
    let cfg = crate::config::get();

    let stderr_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("clip_llm=info"));
    let fmt_layer = fmt::layer().with_filter(stderr_filter);

    let victoria = cfg.logging_url().map(|url| {
        let level = parse_level(cfg.logging_level().unwrap_or("info"));
        let vcfg = victoria::VictoriaConfig {
            url: url.to_string(),
            batch_max: cfg.logging_batch_max().unwrap_or(200).max(1),
            flush: Duration::from_millis(cfg.logging_flush_ms().unwrap_or(2000).max(1)),
        };
        // Scope to the crate's own target: excludes reqwest/hyper (and so the
        // shipper's own requests), preventing a ship → log → ship loop.
        let filter = Targets::new().with_target("clip_llm", level);
        victoria::VictoriaLayer::new(vcfg).with_filter(filter)
    });

    // `Option<Layer>` is itself a `Layer` (a no-op when `None`), so the same
    // builder handles both the enabled and disabled cases.
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(victoria)
        .init();

    if let Some(url) = cfg.logging_url() {
        tracing::info!(
            target: "clip_llm::telemetry",
            level = %cfg.logging_level().unwrap_or("info"),
            "VictoriaLogs sink enabled: {url}"
        );
    }
}

/// Parse a `[logging].level` string into a `LevelFilter`; unknown → `info`.
fn parse_level(s: &str) -> LevelFilter {
    match s.to_ascii_lowercase().as_str() {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "warn" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        _ => LevelFilter::INFO,
    }
}
