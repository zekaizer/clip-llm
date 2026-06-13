//! VictoriaLogs remote log sink.
//!
//! A `tracing` [`Layer`] that serializes each event — merged with the field set
//! of every span in its scope — into newline-delimited JSON and ships it to a
//! VictoriaLogs `/insert/jsonline` endpoint from a dedicated background thread.
//!
//! Design constraints (see the feature design):
//! - **Never blocks app threads.** `on_event` only formats + `try_send`s into a
//!   bounded channel; a full channel drops the record (and bumps a counter)
//!   rather than blocking the thread that emitted the log.
//! - **Never recurses.** The shipper uses `reqwest` (whose `reqwest`/`hyper`
//!   targets are excluded by the layer's per-layer `Targets` filter) and reports
//!   its own failures with `eprintln!`, never through `tracing`.
//! - **No extra dependencies.** Time is sent as a Unix-nanosecond number (which
//!   VictoriaLogs accepts for `_time_field`), avoiding a date-formatting crate;
//!   the shipper drives async `reqwest` on its own current-thread runtime.

use std::fmt::Debug;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Bounded buffer between the layer (any thread) and the shipper thread.
/// A full buffer drops records — remote logging must never stall the app.
const CHANNEL_CAP: usize = 8192;

/// Records dropped because the channel was full, surfaced periodically by the
/// shipper. A process-global counter so the layer and shipper share it without
/// threading a reference through both.
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Resolved configuration for the VictoriaLogs sink.
pub struct VictoriaConfig {
    /// Base URL, e.g. `http://192.168.1.15:9428` (trailing slash tolerated).
    pub url: String,
    /// Flush once this many records are buffered.
    pub batch_max: usize,
    /// Flush at least this often even if `batch_max` is not reached.
    pub flush: Duration,
}

/// Per-span captured field set, stored in the span's extensions so an event can
/// inherit the fields of every span in its scope.
struct SpanFields(Map<String, Value>);

/// Collects `tracing` field values into a JSON object. The reserved `message`
/// field is stored under `msg` to match the `_msg_field=msg` ingestion param.
struct JsonVisitor<'a> {
    map: &'a mut Map<String, Value>,
}

impl JsonVisitor<'_> {
    fn put(&mut self, field: &Field, value: Value) {
        let key = match field.name() {
            "message" => "msg",
            other => other,
        };
        self.map.insert(key.to_string(), value);
    }
}

impl Visit for JsonVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, Value::from(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::from(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Value::from(value));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.put(field, Value::from(format!("{value:?}")));
    }
}

/// The `tracing` layer. Holds the static resource fields attached to every line
/// and the sender half of the channel to the shipper thread.
pub struct VictoriaLayer {
    tx: SyncSender<String>,
    app: &'static str,
    version: &'static str,
    host: String,
    session: String,
}

impl VictoriaLayer {
    /// Build the layer and spawn the background shipper thread.
    pub fn new(cfg: VictoriaConfig) -> Self {
        let (tx, rx) = sync_channel::<String>(CHANNEL_CAP);
        spawn_shipper(cfg, rx);
        Self {
            tx,
            app: "clip-llm",
            version: env!("CARGO_PKG_VERSION"),
            host: detect_host(),
            session: gen_session(),
        }
    }
}

impl<S> Layer<S> for VictoriaLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut map = Map::new();
        attrs.record(&mut JsonVisitor { map: &mut map });
        span.extensions_mut().insert(SpanFields(map));
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut ext = span.extensions_mut();
        if let Some(SpanFields(map)) = ext.get_mut::<SpanFields>() {
            values.record(&mut JsonVisitor { map });
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut map = Map::new();

        // Event time as Unix nanoseconds — VictoriaLogs accepts a numeric Unix
        // timestamp for `_time_field` and auto-detects the unit, so no
        // RFC3339 formatting (and no date crate) is needed.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        map.insert("time".into(), Value::from(nanos));
        map.insert("level".into(), Value::from(meta.level().as_str()));
        map.insert("target".into(), Value::from(meta.target()));
        if let Some(file) = meta.file() {
            map.insert("file".into(), Value::from(file));
        }
        if let Some(line) = meta.line() {
            map.insert("line".into(), Value::from(line));
        }

        // Static resource fields (also the stream-identity fields).
        map.insert("app".into(), Value::from(self.app));
        map.insert("version".into(), Value::from(self.version));
        map.insert("host".into(), Value::from(self.host.clone()));
        map.insert("session".into(), Value::from(self.session.clone()));

        // Inherit fields from every span in scope (root → leaf, so the innermost
        // span wins on a name clash); record the leaf span name under `span`.
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(fields) = span.extensions().get::<SpanFields>() {
                    for (k, v) in &fields.0 {
                        map.insert(k.clone(), v.clone());
                    }
                }
                map.insert("span".into(), Value::from(span.name()));
            }
        }

        // The event's own fields override inherited span fields on a clash.
        event.record(&mut JsonVisitor { map: &mut map });

        if let Ok(line) = serde_json::to_string(&Value::Object(map)) {
            match self.tx.try_send(line) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    DROPPED.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }
}

/// Background thread: batch buffered records and POST them to VictoriaLogs.
fn spawn_shipper(cfg: VictoriaConfig, rx: Receiver<String>) {
    let url = format!(
        "{}/insert/jsonline?_stream_fields=app,host,session&_msg_field=msg&_time_field=time",
        cfg.url.trim_end_matches('/')
    );
    let builder = std::thread::Builder::new().name("victoria-logs".into());
    let spawned = builder.spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("victoria-logs: failed to build runtime: {e}");
                return;
            }
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap_or_default();
        let mut last_dropped = 0u64;

        // Block until at least one record is ready; the loop ends when the
        // channel is dropped (process shutdown). The layer's sender is
        // process-global, so in practice this blocks idly rather than exiting.
        while let Ok(first) = rx.recv() {
            let mut body = String::with_capacity(8192);
            body.push_str(&first);
            body.push('\n');

            // Drain further records into the batch until batch_max or the flush
            // window elapses.
            let deadline = Instant::now() + cfg.flush;
            let mut count = 1;
            while count < cfg.batch_max {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                match rx.recv_timeout(deadline - now) {
                    Ok(line) => {
                        body.push_str(&line);
                        body.push('\n');
                        count += 1;
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }

            let result = rt.block_on(async {
                client
                    .post(&url)
                    .header("Content-Type", "application/stream+json")
                    .body(body)
                    .send()
                    .await
            });
            match result {
                Ok(resp) if resp.status().is_success() => {}
                Ok(resp) => eprintln!("victoria-logs: ingest rejected (HTTP {})", resp.status().as_u16()),
                Err(e) => eprintln!("victoria-logs: ingest failed: {e}"),
            }

            // Surface newly-dropped records (channel was full) once per flush.
            let dropped = DROPPED.load(Ordering::Relaxed);
            if dropped != last_dropped {
                eprintln!(
                    "victoria-logs: dropped {} record(s) (buffer full)",
                    dropped - last_dropped
                );
                last_dropped = dropped;
            }
        }
    });
    if let Err(e) = spawned {
        eprintln!("victoria-logs: failed to spawn shipper thread: {e}");
    }
}

/// Best-effort host name for the `host` stream field, with no extra dependency:
/// the platform env var first, then the `hostname` command, then `"unknown"`.
fn detect_host() -> String {
    if let Ok(h) = std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME"))
        && !h.is_empty()
    {
        return h;
    }
    if let Ok(out) = std::process::Command::new("hostname").output()
        && out.status.success()
    {
        let h = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !h.is_empty() {
            return h;
        }
    }
    "unknown".to_string()
}

/// Short random-ish id identifying this process run, for correlating all logs
/// from one launch. Derived from the start time and pid — no `rand`/`uuid` dep.
fn gen_session() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mixed = nanos ^ ((std::process::id() as u64) << 32);
    format!("{:08x}", mixed as u32 ^ (mixed >> 32) as u32)
}
