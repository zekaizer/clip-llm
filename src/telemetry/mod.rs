//! Tracing setup: console logging plus the optional remote telemetry sink.
//!
//! Console (stderr) logging via the `fmt` layer is always on. When
//! `[telemetry].url` is set, a second [`victoria::VictoriaLayer`] is added that
//! ships structured events to a VictoriaLogs instance (opt-in; see module docs).
//!
//! Each sink carries its own per-layer filter so they are independent: stderr
//! keeps the usual `clip_llm=info` (or `RUST_LOG`) level, while the telemetry
//! sink ships `clip_llm` events at `[telemetry].level` (default `info`). The
//! telemetry filter is scoped to the `clip_llm` target so dependency logs
//! (reqwest/hyper) are never shipped — which also prevents the shipper's own
//! HTTP from recursing back into the sink.

mod victoria;

/// `(shipped, dropped)` record counts for the telemetry sink — surfaced in the
/// tray Status menu. Both zero when the sink is disabled.
pub use victoria::counts as telemetry_counts;

use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

/// Initialize the global tracing subscriber. Reads `[telemetry]` from the
/// loaded config, so `crate::config::init()` must run first.
pub fn init() {
    let cfg = crate::config::get();

    let stderr_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("clip_llm=info"));
    let fmt_layer = fmt::layer().with_filter(stderr_filter);

    let telemetry = cfg.telemetry_url().map(|url| {
        let level = parse_level(cfg.telemetry_level().unwrap_or("info"));
        let vcfg = victoria::VictoriaConfig {
            url: url.to_string(),
            batch_max: cfg.telemetry_batch_max().unwrap_or(200).max(1),
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
        .with(telemetry)
        .init();

    if let Some(url) = cfg.telemetry_url() {
        tracing::info!(
            target: "clip_llm::telemetry",
            level = %cfg.telemetry_level().unwrap_or("info"),
            "telemetry sink enabled (VictoriaLogs): {url}"
        );
    }
}

/// Parses a `[telemetry].level` string into a `LevelFilter`.
///
/// An unrecognized value falls back to `info`, but — unlike a silent
/// fallback — prints a warning naming both the offending value and the
/// fallback level: a typo here (e.g. `"eror"` instead of `"error"`) would
/// otherwise ship far more verbose logs than the user intended, with no
/// indication why. `eprintln!` is used because tracing is not yet
/// initialized when this runs (this function feeds `init()` itself).
fn parse_level(s: &str) -> LevelFilter {
    match s.to_ascii_lowercase().as_str() {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "info" => LevelFilter::INFO,
        "warn" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        _ => {
            eprintln!(
                "clip-llm: [telemetry].level: unrecognized value {s:?} — falling back to \"info\""
            );
            LevelFilter::INFO
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_level_recognizes_all_known_levels() {
        assert_eq!(parse_level("trace"), LevelFilter::TRACE);
        assert_eq!(parse_level("debug"), LevelFilter::DEBUG);
        assert_eq!(parse_level("info"), LevelFilter::INFO);
        assert_eq!(parse_level("warn"), LevelFilter::WARN);
        assert_eq!(parse_level("error"), LevelFilter::ERROR);
        // Case-insensitive.
        assert_eq!(parse_level("ERROR"), LevelFilter::ERROR);
    }

    #[test]
    fn parse_level_falls_back_to_info_on_typo() {
        // A typo (e.g. "eror" for "error") must not silently become the far
        // more verbose default; it still falls back to info (the warning
        // itself goes to stderr, which this test does not capture).
        assert_eq!(parse_level("eror"), LevelFilter::INFO);
        assert_eq!(parse_level(""), LevelFilter::INFO);
    }
}
