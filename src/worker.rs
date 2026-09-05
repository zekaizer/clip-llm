use std::sync::mpsc;
use std::thread;
use std::time::{Instant, SystemTime};

use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error, info, trace, warn, Instrument};

use crate::api::client::{LlmClient, RetryNotice, SseEvent, SseParser, Usage};
use crate::api::response::{extract_first_think_content, strip_think_blocks, ThinkBlockFilter};
use crate::{ApiError, ClipboardContent, DebugCapture, ProcessMode, RephraseParams, ThinkingMode};

/// Maximum time to wait for the next streaming chunk before treating the stream
/// as stalled. The streaming HTTP client has only a connect timeout (no overall
/// body timeout), so without this a server that connects and then goes silent
/// would leave the request hanging forever on a permanent spinner.
const STREAM_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Runtime configuration resolved from environment variables once at worker startup.
#[derive(Copy, Clone)]
struct WorkerConfig {
    /// Use streaming SSE API. Disabled by setting `CLIP_LLM_NO_STREAM`, else taken
    /// from the config `[api].streaming` (default true).
    streaming: bool,
    /// Use mock LLM responses for diagnostics. Enabled by setting `DIAG_MOCK`.
    #[cfg(feature = "diagnostics")]
    use_mock: bool,
}

/// Bundled parameters for a single LLM processing request.
pub struct ProcessTask {
    pub content: ClipboardContent,
    pub mode: ProcessMode,
    pub rephrase_params: RephraseParams,
    pub thinking_mode: ThinkingMode,
    pub request_id: u64,
}

pub enum WorkerCommand {
    Process(ProcessTask),
    Cancel,
}

pub enum WorkerResponse {
    StreamDelta { text: String, request_id: u64 },
    /// Emitted once when the first `<think>` block begins (streaming only).
    ThinkStarted { request_id: u64 },
    /// The client scheduled an automatic retry of this request (transient
    /// failure or 429) and is waiting `delay` before resending.
    Retrying {
        request_id: u64,
        attempt: u32,
        max_attempts: u32,
        delay: std::time::Duration,
        rate_limited: bool,
    },
    /// A finished response. `incomplete` is `Some(reason)` when the stream was
    /// cut short (token limit, stall, dropped connection, transport error) but
    /// partial content was received — the partial is shown with the reason as a
    /// banner instead of being discarded. `None` for a clean completion.
    Complete {
        result: String,
        think_content: Option<String>,
        request_id: u64,
        incomplete: Option<String>,
        /// Raw request/response snapshot for the on-demand debug view.
        debug: DebugCapture,
    },
    Error {
        message: String,
        request_id: u64,
        /// Raw request/response snapshot for the on-demand debug view.
        debug: DebugCapture,
    },
    /// One-shot: vision + thinking-control capability from the startup probe
    /// (sent once). Drives both the state machine's thinking flag and the tray
    /// Status menu.
    ProbeComplete {
        vision_supported: bool,
        thinking_method: crate::api::client::ThinkingControlMethod,
    },
}

/// Map a reqwest transport error to a short, user-facing message that hides
/// internal endpoint URLs and OS error codes. The full error is logged separately.
fn friendly_reqwest_error(e: &reqwest::Error) -> String {
    if e.is_connect() || e.is_timeout() {
        "Cannot reach the server. Check that the LLM service is running.".to_string()
    } else if let Some(status) = e.status() {
        friendly_status_message(status)
    } else if e.is_request() {
        "Configuration error: invalid server URL.".to_string()
    } else {
        "Network error.".to_string()
    }
}

/// Map an HTTP status to an actionable user-facing message. Common 4xx codes
/// get specific guidance (what to check/do); the rest stay generic.
fn friendly_status_message(status: reqwest::StatusCode) -> String {
    use reqwest::StatusCode;
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            "Authentication failed. Check the API key in config.toml.".to_string()
        }
        StatusCode::NOT_FOUND => {
            "Endpoint not found. Check the server URL in config.toml.".to_string()
        }
        StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE => {
            "The server rejected the request — the text may be too long. \
             Try a shorter selection."
                .to_string()
        }
        StatusCode::TOO_MANY_REQUESTS => {
            "Rate limited by the server. Try again in a moment.".to_string()
        }
        s if s.is_client_error() => {
            format!("Server rejected the request (HTTP {}).", s.as_u16())
        }
        s if s.is_server_error() => "Server error. Try again in a moment.".to_string(),
        _ => "Network error.".to_string(),
    }
}

/// Map an [`ApiError`] to a short, user-facing message (no internal details).
fn friendly_api_error(e: &ApiError) -> String {
    match e {
        ApiError::Http(inner) => friendly_reqwest_error(inner),
        // Reaches the user only if a client is built lazily post-startup;
        // normally a missing required setting fails fast at startup instead.
        ApiError::MissingConfig(name) => {
            format!("Required setting not configured: {name}. Set it in config.toml.")
        }
        ApiError::InitialResponseTimeout(_) => {
            "The server is slow to respond. Try again in a moment.".to_string()
        }
        ApiError::EmptyResponse => {
            "The model returned no visible output. Try switching mode or disabling thinking."
                .to_string()
        }
        ApiError::MalformedResponse(_) => {
            "The server returned an unreadable response. Check the endpoint address (details in debug copy)."
                .to_string()
        }
        ApiError::NoUsableContent => {
            "Image cannot be processed — the connected model does not support vision.".to_string()
        }
        ApiError::Truncated => {
            "The reply was cut off by the token limit. Increase [generation] max_tokens or shorten the input."
                .to_string()
        }
        ApiError::Cancelled => "Request cancelled.".to_string(),
        // Auth messages are written user-facing at the source (they name the
        // recovery step, e.g. re-running the provider's CLI login).
        ApiError::Auth(message) => message.clone(),
        ApiError::InvalidConfig(message) => {
            format!("Configuration error: {message}. Fix it in config.toml.")
        }
    }
}

/// Strip think blocks from raw LLM output and build the appropriate response.
/// Logs completion with the given `label` (e.g. "complete", "stream complete").
fn make_complete_response(
    raw: &str,
    mode: ProcessMode,
    request_id: u64,
    label: &str,
    incomplete: Option<String>,
    debug: DebugCapture,
) -> WorkerResponse {
    let think_content = extract_first_think_content(raw);
    let text = strip_think_blocks(raw);
    if text.is_empty() {
        // Nothing usable arrived. If the stream was cut short, report that
        // reason; otherwise the model genuinely produced no visible output.
        let message = incomplete.unwrap_or_else(|| {
            "The model returned no visible output. Try switching mode or disabling thinking."
                .to_string()
        });
        WorkerResponse::Error { message, request_id, debug }
    } else {
        match &incomplete {
            Some(reason) => warn!(
                "worker: {} {label} INCOMPLETE ({} chars): {reason}",
                mode.label(),
                text.len()
            ),
            None => info!("worker: {} {label} ({} chars)", mode.label(), text.len()),
        }
        WorkerResponse::Complete {
            result: text,
            think_content,
            request_id,
            incomplete,
            debug,
        }
    }
}

/// Stamp a streaming capture at a finalize point: elapsed time, the raw SSE
/// stream received so far, and an optional reason the stream ended abnormally.
/// Reached only on the streaming success path, where `send_request` has already
/// cleared `response_raw` to `None`, so the accumulated SSE is set unconditionally
/// (an empty stream finalizes to an empty body, never a stale prior error).
fn finalize_stream_capture(
    capture: &mut DebugCapture,
    started: Instant,
    raw_sse: &mut Vec<u8>,
    error: Option<&str>,
) {
    capture.elapsed_ms = Some(started.elapsed().as_millis());
    // Convert once over the whole body: per-chunk lossy conversion would put a
    // U+FFFD wherever a multi-byte char happened to straddle a chunk boundary.
    capture.response_raw = Some(String::from_utf8_lossy(&std::mem::take(raw_sse)).into_owned());
    if let Some(reason) = error {
        capture.error = Some(reason.to_string());
    }
}

/// Mutable state threaded through the SSE parsing loop in `run_streaming`,
/// accumulated event-by-event by [`handle_stream_event`] and read/closed out
/// by [`finalize_stream`]. Bundling these fields (previously separate
/// parameters) keeps both functions' signatures manageable.
#[derive(Default)]
struct StreamState {
    /// Accumulated content in wire order: reasoning tokens wrapped in a
    /// synthetic `<think>…</think>` block, followed by answer tokens.
    full_content: String,
    /// `true` while inside a reasoning block reconstructed from a separate
    /// `reasoning_content`/`reasoning` SSE field (see [`handle_stream_event`]).
    reasoning_open: bool,
    filter: ThinkBlockFilter,
    /// Set once `ThinkStarted` has been sent, so it fires at most once.
    think_notified: bool,
    /// The server's reported stop reason (e.g. `"stop"`, `"length"`), if any.
    finish_reason: Option<String>,
    /// Token usage, when a chunk carried it (see [`SseEvent::Usage`]).
    usage: Option<Usage>,
    /// Server-reported mid-stream failure reason (see [`SseEvent::Fatal`]).
    fatal: Option<String>,
}

/// Reason a finished stream should be presented as incomplete: a server-
/// reported failure wins, then a token-limit stop reason. `None` = clean.
fn stream_incomplete_reason(state: &StreamState) -> Option<String> {
    if let Some(reason) = &state.fatal {
        return Some(format!("The server reported an error: {reason}"));
    }
    (state.finish_reason.as_deref() == Some("length"))
        .then(|| friendly_api_error(&ApiError::Truncated))
}

/// Process one parsed SSE event into streaming state, emitting `ThinkStarted`
/// and `StreamDelta` responses as needed. Returns `true` when a terminal
/// `[DONE]` marker was seen (the caller then finalizes the response).
///
/// Reasoning tokens (servers running a reasoning parser — e.g. vLLM for Qwen3 —
/// deliver them in a separate `reasoning_content` field, ahead of the answer)
/// are wrapped back into a leading `<think>…</think>` block inside
/// `state.full_content`. This keeps the existing think-block extraction,
/// stripping, and partial-recovery paths working with a single uniform
/// representation.
fn handle_stream_event(
    event: SseEvent,
    state: &mut StreamState,
    resp_tx: &mpsc::Sender<WorkerResponse>,
    request_id: u64,
) -> bool {
    match event {
        SseEvent::Reasoning(token) => {
            if !state.reasoning_open {
                state.full_content.push_str("<think>");
                state.reasoning_open = true;
            }
            state.full_content.push_str(&token);
            if !state.think_notified {
                state.think_notified = true;
                let _ = resp_tx.send(WorkerResponse::ThinkStarted { request_id });
            }
        }
        SseEvent::Content(token) => {
            // First answer token after reasoning closes the think block.
            if state.reasoning_open {
                state.full_content.push_str("</think>");
                state.reasoning_open = false;
            }
            state.full_content.push_str(&token);
            let visible = state.filter.feed(&token);
            // Inline `<think>` path (no reasoning parser): notify on first
            // think content the filter detects.
            if state.filter.has_think_content() && !state.think_notified {
                state.think_notified = true;
                let _ = resp_tx.send(WorkerResponse::ThinkStarted { request_id });
            }
            if !visible.is_empty() {
                let _ = resp_tx.send(WorkerResponse::StreamDelta {
                    text: visible,
                    request_id,
                });
            }
        }
        SseEvent::Finish(reason) => {
            state.finish_reason = Some(reason);
        }
        SseEvent::Usage(usage) => {
            state.usage = Some(usage);
        }
        SseEvent::Fatal(reason) => {
            // Terminal like Done, but the completion must carry the server's
            // stated reason (partial text shows with it as the banner; no
            // text at all becomes an error with it as the message).
            warn!("worker: server reported stream failure: {reason}");
            state.fatal = Some(reason);
            return true;
        }
        SseEvent::Done => return true,
    }
    false
}

/// Finalize a streaming response at one of `run_streaming`'s exit points:
/// close a dangling `<think>` tag left open by an unterminated reasoning
/// block, stamp the debug capture (elapsed time, raw SSE received so far, and
/// an optional error reason for the debug view), build the completion
/// response via [`make_complete_response`], and send it.
///
/// Centralizes a sequence that used to be repeated at 5+ exit points: idle
/// timeout, `[DONE]`, flushed `[DONE]`, EOF with/without a clean `stop`
/// reason, and a chunk transport error. Each call site differs only in the
/// `label`, `incomplete` (user-facing partial-result banner), and
/// `debug_error` (raw reason recorded in the debug capture) it passes.
#[allow(clippy::too_many_arguments)]
fn finalize_stream(
    state: &mut StreamState,
    mode: ProcessMode,
    request_id: u64,
    label: &str,
    incomplete: Option<String>,
    debug_error: Option<&str>,
    mut capture: DebugCapture,
    started: Instant,
    raw_sse: &mut Vec<u8>,
    resp_tx: &mpsc::Sender<WorkerResponse>,
) {
    if state.reasoning_open {
        state.full_content.push_str("</think>");
        state.reasoning_open = false;
    }
    if let Some(usage) = state.usage {
        capture.prompt_tokens = Some(usage.prompt_tokens);
        capture.completion_tokens = Some(usage.completion_tokens);
        capture.total_tokens = Some(usage.total_tokens);
        info!(
            prompt_tokens = usage.prompt_tokens,
            completion_tokens = usage.completion_tokens,
            total_tokens = usage.total_tokens,
            mode = mode.label(),
            "token usage"
        );
    }
    finalize_stream_capture(&mut capture, started, raw_sse, debug_error);
    let r = make_complete_response(&state.full_content, mode, request_id, label, incomplete, capture);
    let _ = resp_tx.send(r);
}

/// Non-streaming LLM request: single request/response with cancellation support.
async fn run_non_streaming(
    llm: LlmClient,
    task: ProcessTask,
    resp_tx: mpsc::Sender<WorkerResponse>,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let ProcessTask { content, mode, rephrase_params, thinking_mode, request_id } = task;
    let mut capture = DebugCapture {
        timestamp: Some(crate::format_utc(SystemTime::now())),
        ..Default::default()
    };
    let started = Instant::now();
    let result = tokio::select! {
        r = llm.complete(&content, mode, rephrase_params, thinking_mode, &mut capture) => r,
        _ = &mut cancel_rx => {
            debug!("worker: request cancelled during connect");
            return;
        }
    };
    capture.elapsed_ms = Some(started.elapsed().as_millis());
    let r = match result {
        Ok(raw) => make_complete_response(&raw, mode, request_id, "complete", None, capture),
        Err(e) => {
            error!("worker: LLM error: {e}");
            capture.error = Some(format!("{e:?}"));
            WorkerResponse::Error {
                message: friendly_api_error(&e),
                request_id,
                debug: capture,
            }
        }
    };
    let _ = resp_tx.send(r);
}

/// Streaming LLM request: SSE parsing loop with incremental delta delivery.
async fn run_streaming(
    llm: LlmClient,
    task: ProcessTask,
    resp_tx: mpsc::Sender<WorkerResponse>,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let ProcessTask { content, mode, rephrase_params, thinking_mode, request_id } = task;
    let mut capture = DebugCapture {
        timestamp: Some(crate::format_utc(SystemTime::now())),
        ..Default::default()
    };
    let started = Instant::now();
    let resp = tokio::select! {
        r = llm.complete_stream(&content, mode, rephrase_params, thinking_mode, &mut capture) => r,
        _ = &mut cancel_rx => {
            debug!("worker: request cancelled during connect");
            return;
        }
    };

    let mut resp = match resp {
        Ok(r) => r,
        Err(e) => {
            error!("worker: LLM stream error: {e}");
            // A non-2xx rejection already stored its body/status in `capture`.
            capture.elapsed_ms = Some(started.elapsed().as_millis());
            capture.error = Some(format!("{e:?}"));
            let _ = resp_tx.send(WorkerResponse::Error {
                message: friendly_api_error(&e),
                request_id,
                debug: capture,
            });
            return;
        }
    };

    // Diagnostic: the negotiated HTTP version and framing pin down a proxy that
    // mishandles SSE (HTTP/2 early END_STREAM, response buffering, missing
    // chunked transfer) versus the server itself.
    debug!(
        "stream response opened: version={:?}, status={}, content_type={:?}, transfer_encoding={:?}, connection={:?}",
        resp.version(),
        resp.status().as_u16(),
        resp.headers().get(reqwest::header::CONTENT_TYPE),
        resp.headers().get(reqwest::header::TRANSFER_ENCODING),
        resp.headers().get(reqwest::header::CONNECTION),
    );

    let mut parser = SseParser::new();
    let mut state = StreamState::default();
    // Diagnostic counters: how much arrived before any early close, so the logs
    // distinguish "died after the first frame" from a genuine mid-reply cut.
    let mut chunk_count: u32 = 0;
    let mut byte_count: usize = 0;
    // Raw SSE bytes as received, accumulated for the debug view (the parsed
    // visible text in `state.full_content` is not the wire data). Kept as
    // bytes and converted once at finalize, so a multi-byte char straddling a
    // chunk boundary is not mangled into U+FFFD by per-chunk conversion.
    let mut raw_sse: Vec<u8> = Vec::new();

    loop {
        let chunk = tokio::select! {
            c = tokio::time::timeout(STREAM_IDLE_TIMEOUT, resp.chunk()) => match c {
                Ok(chunk) => chunk,
                Err(_elapsed) => {
                    warn!(
                        "worker: stream idle for {}s after {chunk_count} chunks/{byte_count} bytes — treating as stalled",
                        STREAM_IDLE_TIMEOUT.as_secs()
                    );
                    // Keep whatever arrived before the stall (#65).
                    finalize_stream(
                        &mut state, mode, request_id, "stream stalled",
                        Some("The server stopped responding mid-reply. Try again.".to_string()),
                        Some("stream idle timeout — server stopped responding mid-reply"),
                        capture, started, &mut raw_sse, &resp_tx,
                    );
                    return;
                }
            },
            _ = &mut cancel_rx => {
                debug!("worker: request cancelled during streaming");
                return;
            }
        };

        match chunk {
            Ok(Some(bytes)) => {
                chunk_count += 1;
                byte_count += bytes.len();
                if tracing::enabled!(tracing::Level::TRACE) {
                    trace!(
                        "SSE raw chunk #{chunk_count} ({} bytes):\n{}",
                        bytes.len(),
                        String::from_utf8_lossy(&bytes)
                    );
                }
                raw_sse.extend_from_slice(&bytes);
                for event in parser.feed(&bytes) {
                    let done = handle_stream_event(event, &mut state, &resp_tx, request_id);
                    if done {
                        // Terminal event received. "length" means generation
                        // hit max_tokens, a Fatal carries a server-reported
                        // failure — show the partial with the reason as a
                        // banner instead of dropping it (#60, #65).
                        debug!(
                            "SSE stream done, full content ({} chars):\n{}",
                            state.full_content.len(), state.full_content
                        );
                        let incomplete = stream_incomplete_reason(&state);
                        finalize_stream(
                            &mut state, mode, request_id, "stream complete",
                            incomplete.clone(), incomplete.as_deref(),
                            capture, started, &mut raw_sse, &resp_tx,
                        );
                        return;
                    }
                }
            }
            Ok(None) => {
                // Body ended. First flush any final line the server sent without
                // a trailing newline (a [DONE] or finish chunk feed() couldn't
                // parse). A flushed [DONE] finalizes exactly like the inline arm.
                for event in parser.flush() {
                    if handle_stream_event(event, &mut state, &resp_tx, request_id) {
                        let incomplete = stream_incomplete_reason(&state);
                        finalize_stream(
                            &mut state, mode, request_id, "stream complete (flushed)",
                            incomplete.clone(), incomplete.as_deref(),
                            capture, started, &mut raw_sse, &resp_tx,
                        );
                        return;
                    }
                }
                // No [DONE], but the server may have already reported why it
                // stopped. finish_reason="stop" => a clean completion whose
                // [DONE] line never arrived (proxy dropped it / server closed
                // early); treat it as success. Only a missing or non-stop reason
                // is a genuinely truncated connection (proxy/server cut mid-reply,
                // #60) — show the partial with a banner instead of dropping it (#65).
                match state.finish_reason.as_deref() {
                    Some("stop") => {
                        debug!(
                            "stream EOF with finish_reason=stop but no [DONE] ({} chars) — treating as complete",
                            state.full_content.len()
                        );
                        finalize_stream(
                            &mut state, mode, request_id, "stream complete (no DONE)", None, None,
                            capture, started, &mut raw_sse, &resp_tx,
                        );
                    }
                    other => {
                        warn!(
                            "worker: stream closed without [DONE] after {chunk_count} chunks/{byte_count} bytes, {} chars (finish_reason={other:?}) — treating as truncated",
                            state.full_content.len()
                        );
                        let incomplete = if other == Some("length") {
                            friendly_api_error(&ApiError::Truncated)
                        } else {
                            "The connection closed before the reply finished. Try again.".to_string()
                        };
                        let debug_error = format!("stream closed early (finish_reason={other:?})");
                        finalize_stream(
                            &mut state, mode, request_id, "stream closed early", Some(incomplete),
                            Some(&debug_error),
                            capture, started, &mut raw_sse, &resp_tx,
                        );
                    }
                }
                return;
            }
            Err(e) => {
                error!("worker: stream chunk error after {chunk_count} chunks/{byte_count} bytes: {e}");
                let debug_error = format!(
                    "stream chunk error after {chunk_count} chunks/{byte_count} bytes: {e:?}"
                );
                finalize_stream(
                    &mut state, mode, request_id, "stream error",
                    Some(friendly_reqwest_error(&e)), Some(&debug_error),
                    capture, started, &mut raw_sse, &resp_tx,
                );
                return;
            }
        }
    }
}

/// Mock streaming for diagnostics: simulate chunked delivery with canned responses.
#[cfg(feature = "diagnostics")]
async fn run_mock_streaming(
    mock_text: String,
    mode: ProcessMode,
    request_id: u64,
    resp_tx: mpsc::Sender<WorkerResponse>,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
) {
    // Recorded so the mock path exercises the same Result-bottom-row
    // completion status (`format_completion_status` in `src/ui/mod.rs`) that
    // a real request does — otherwise `DebugCapture::default()`'s missing
    // `elapsed_ms` leaves that row visibly blank in diagnostics screenshots.
    let started = std::time::Instant::now();
    match crate::diagnostics::mock_response(&mock_text) {
        Ok(mock) => {
            let chunks: Vec<&str> = mock.split_inclusive(char::is_whitespace).collect();
            for (i, chunk) in chunks.iter().enumerate() {
                if i > 0 {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(30)) => {}
                        _ = &mut cancel_rx => {
                            debug!("worker: mock cancelled");
                            return;
                        }
                    }
                }
                let _ = resp_tx.send(WorkerResponse::StreamDelta {
                    text: chunk.to_string(),
                    request_id,
                });
            }
            info!("worker: mock {} complete ({} chars)", mode.label(), mock.len());
            let _ = resp_tx.send(WorkerResponse::Complete {
                result: mock,
                think_content: None,
                request_id,
                incomplete: None,
                debug: DebugCapture {
                    elapsed_ms: Some(started.elapsed().as_millis()),
                    ..Default::default()
                },
            });
        }
        Err(msg) => {
            info!("worker: mock error: {msg}");
            let _ = resp_tx.send(WorkerResponse::Error {
                message: msg,
                request_id,
                debug: DebugCapture {
                    elapsed_ms: Some(started.elapsed().as_millis()),
                    ..Default::default()
                },
            });
        }
    }
}

/// Handle a Process command: cancel any in-flight request, then spawn the
/// appropriate async task (mock / non-streaming / streaming).
fn dispatch_process(
    task: ProcessTask,
    llm: &LlmClient,
    resp_tx: &mpsc::Sender<WorkerResponse>,
    cancel_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
    config: &WorkerConfig,
) {
    // Cancel any in-flight request.
    if let Some(tx) = cancel_tx.take() {
        let _ = tx.send(());
        debug!("cancelled previous in-flight request");
    }

    let (c_tx, c_rx) = tokio::sync::oneshot::channel();
    *cancel_tx = Some(c_tx);

    let resp_tx = resp_tx.clone();
    let llm = {
        let resp_tx = resp_tx.clone();
        let request_id = task.request_id;
        llm.with_retry_notifier(std::sync::Arc::new(move |n: RetryNotice| {
            let _ = resp_tx.send(WorkerResponse::Retrying {
                request_id,
                attempt: n.attempt,
                max_attempts: n.max_attempts,
                delay: n.delay,
                rate_limited: n.rate_limited,
            });
        }))
    };

    let text_len = task.content.text.as_ref().map_or(0, |t| t.len());
    let img_count = task.content.images.len();

    // One span per request: every log emitted while the request runs inherits
    // these fields, so a request can be filtered/correlated end to end in the
    // remote logs (and they show in the stderr span context too).
    let span = tracing::info_span!(
        "request",
        request_id = task.request_id,
        mode = task.mode.label(),
        streaming = config.streaming,
        text_len,
        img_count,
    );

    // Mock mode: simulate streaming with canned responses.
    #[cfg(feature = "diagnostics")]
    if config.use_mock {
        let mode = task.mode;
        let request_id = task.request_id;
        let mock_text = task.content.text.unwrap_or_default();
        info!(parent: &span, "worker: mock streaming {} ({} chars)", mode.label(), mock_text.len());
        tokio::spawn(run_mock_streaming(mock_text, mode, request_id, resp_tx, c_rx).instrument(span));
        return;
    }

    if config.streaming {
        info!(parent: &span, "worker: starting stream {} ({text_len} chars, {img_count} images)", task.mode.label());
        tokio::spawn(run_streaming(llm, task, resp_tx, c_rx).instrument(span));
    } else {
        info!(parent: &span, "worker: starting {} ({text_len} chars, {img_count} images, no-stream)", task.mode.label());
        tokio::spawn(run_non_streaming(llm, task, resp_tx, c_rx).instrument(span));
    }
}

/// Spawn a worker thread with a tokio runtime for async LLM calls.
/// Returns the thread handle.
///
/// Uses `tokio::sync::mpsc` for the command channel so that `.recv().await`
/// does not block the single-threaded tokio runtime. `ctx` is used to wake the
/// UI event loop after the one-shot startup probe completes — that response
/// arrives while the overlay is idle (Hidden), so without an explicit repaint
/// the tray Status menu wouldn't update until the next unrelated event. (Other
/// responses arrive during Processing, when the UI already repaints each frame.)
pub fn spawn_worker(
    mut cmd_rx: tokio_mpsc::UnboundedReceiver<WorkerCommand>,
    resp_tx: mpsc::Sender<WorkerResponse>,
    llm: LlmClient,
    ctx: eframe::egui::Context,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime");

        // Read env vars once at thread start — no async context needed.
        // Precedence: CLIP_LLM_NO_STREAM (when set, forces off) > config > default on.
        let config = WorkerConfig {
            streaming: if std::env::var("CLIP_LLM_NO_STREAM").is_ok() {
                false
            } else {
                crate::config::get().api_streaming().unwrap_or(true)
            },
            #[cfg(feature = "diagnostics")]
            use_mock: std::env::var("DIAG_MOCK").is_ok(),
        };

        rt.block_on(async move {
            // Probe vision and thinking support concurrently with the command loop
            // rather than before it: a slow or unreachable server (each probe can
            // take up to the connect timeout, ~30s) must not block the loop from
            // dispatching the first request or honoring Cancel. `build_and_send`
            // re-probes on demand if a request beats the probe, and the `OnceCell`
            // caches the verdict either way.
            tokio::spawn({
                let llm = llm.clone();
                let resp_tx = resp_tx.clone();
                let ctx = ctx.clone();
                async move {
                    let vision_supported = llm.probe_vision().await;
                    let thinking_method = llm.probe_thinking().await;
                    let _ = resp_tx.send(WorkerResponse::ProbeComplete {
                        vision_supported,
                        thinking_method,
                    });
                    // Wake the (idle) UI loop so poll_responses processes this
                    // immediately and the tray Status menu updates without
                    // waiting for the next unrelated event.
                    ctx.request_repaint();
                }
            });

            let mut cancel_tx: Option<tokio::sync::oneshot::Sender<()>> = None;

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    WorkerCommand::Process(task) => {
                        dispatch_process(task, &llm, &resp_tx, &mut cancel_tx, &config);
                    }
                    WorkerCommand::Cancel => {
                        if let Some(tx) = cancel_tx.take() {
                            let _ = tx.send(());
                            info!("worker: cancelled by user");
                        }
                    }
                }
            }

            info!("worker: command channel closed, exiting");
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProcessMode;

    fn assert_complete(r: &WorkerResponse, expected_id: u64) -> String {
        match r {
            WorkerResponse::Complete { result, request_id, .. } => {
                assert_eq!(*request_id, expected_id);
                result.clone()
            }
            other => panic!("expected Complete, got {:?}", variant_name(other)),
        }
    }

    fn assert_error(r: &WorkerResponse, expected_id: u64) -> String {
        match r {
            WorkerResponse::Error { message, request_id, .. } => {
                assert_eq!(*request_id, expected_id);
                message.clone()
            }
            other => panic!("expected Error, got {:?}", variant_name(other)),
        }
    }

    fn variant_name(r: &WorkerResponse) -> &'static str {
        match r {
            WorkerResponse::StreamDelta { .. } => "StreamDelta",
            WorkerResponse::ThinkStarted { .. } => "ThinkStarted",
            WorkerResponse::Retrying { .. } => "Retrying",
            WorkerResponse::Complete { .. } => "Complete",
            WorkerResponse::Error { .. } => "Error",
            WorkerResponse::ProbeComplete { .. } => "ProbeComplete",
        }
    }

    #[test]
    fn make_complete_response_plain_text() {
        let r = make_complete_response("hello world", ProcessMode::Translate, 1, "complete", None, DebugCapture::default());
        let text = assert_complete(&r, 1);
        assert_eq!(text, "hello world");
    }

    #[test]
    fn make_complete_response_strips_think_blocks() {
        let raw = "<think>internal</think>visible output";
        let r = make_complete_response(raw, ProcessMode::Rephrase, 2, "stream complete", None, DebugCapture::default());
        let text = assert_complete(&r, 2);
        assert_eq!(text, "visible output");
    }

    #[test]
    fn make_complete_response_empty_after_strip() {
        let raw = "<think>only thinking</think>";
        let r = make_complete_response(raw, ProcessMode::Summarize, 3, "complete", None, DebugCapture::default());
        let msg = assert_error(&r, 3);
        assert!(msg.contains("no visible output"));
    }

    #[test]
    fn make_complete_response_empty_input() {
        let r = make_complete_response("", ProcessMode::Translate, 4, "complete", None, DebugCapture::default());
        let msg = assert_error(&r, 4);
        assert!(msg.contains("no visible output"));
    }

    #[test]
    fn make_complete_response_whitespace_only_after_strip() {
        let raw = "<think>x</think>   \n  ";
        let r = make_complete_response(raw, ProcessMode::Translate, 5, "complete", None, DebugCapture::default());
        // strip_think_blocks trims whitespace; empty after trim → error.
        let msg = assert_error(&r, 5);
        assert!(msg.contains("no visible output"));
    }

    #[test]
    fn make_complete_response_preserves_request_id() {
        let r = make_complete_response("ok", ProcessMode::Translate, 42, "test", None, DebugCapture::default());
        assert_complete(&r, 42);
    }

    #[test]
    fn make_complete_response_partial_shows_with_reason() {
        // #65: a cut-off reply with content is shown (not dropped), carrying
        // the reason as the `incomplete` banner.
        let r = make_complete_response(
            "partial text", ProcessMode::Translate, 7, "stream", Some("cut off".to_string()),
            DebugCapture::default(),
        );
        match r {
            WorkerResponse::Complete { result, incomplete, request_id, .. } => {
                assert_eq!(request_id, 7);
                assert_eq!(result, "partial text");
                assert_eq!(incomplete.as_deref(), Some("cut off"));
            }
            other => panic!("expected Complete, got {:?}", variant_name(&other)),
        }
    }

    #[test]
    fn make_complete_response_empty_partial_is_error_with_reason() {
        // Cut off before any visible content → error using the cut-off reason.
        let r = make_complete_response(
            "<think>x</think>", ProcessMode::Translate, 8, "stream", Some("cut off".to_string()),
            DebugCapture::default(),
        );
        let msg = assert_error(&r, 8);
        assert_eq!(msg, "cut off");
    }

    // -- handle_stream_event tests --

    use crate::api::client::SseEvent;
    use crate::api::response::{extract_first_think_content, strip_think_blocks};

    #[test]
    fn handle_stream_event_wraps_reasoning_as_think() {
        let (tx, rx) = mpsc::channel();
        let mut state = StreamState::default();

        // Reasoning token opens a think block and fires ThinkStarted once.
        let done = handle_stream_event(SseEvent::Reasoning("why".into()), &mut state, &tx, 1);
        assert!(!done);
        assert!(state.reasoning_open);
        assert!(state.think_notified);
        assert_eq!(state.full_content, "<think>why");

        // The answer token closes the think block and is streamed as visible.
        handle_stream_event(SseEvent::Content("answer".into()), &mut state, &tx, 1);
        assert!(!state.reasoning_open);
        assert_eq!(state.full_content, "<think>why</think>answer");

        // The reconstructed block extracts/strips exactly like an inline one.
        assert_eq!(strip_think_blocks(&state.full_content), "answer");
        assert_eq!(
            extract_first_think_content(&state.full_content).as_deref(),
            Some("why")
        );

        assert!(matches!(rx.try_recv().unwrap(), WorkerResponse::ThinkStarted { .. }));
        assert!(matches!(
            rx.try_recv().unwrap(),
            WorkerResponse::StreamDelta { text, .. } if text == "answer"
        ));
    }

    #[test]
    fn handle_stream_event_fatal_terminates_with_reason() {
        let (tx, rx) = mpsc::channel();
        let mut state = StreamState::default();
        state.full_content.push_str("partial");
        let done =
            handle_stream_event(SseEvent::Fatal("quota exceeded".into()), &mut state, &tx, 1);
        assert!(done, "Fatal must terminate the stream like Done");
        assert!(rx.try_recv().is_err(), "Fatal itself must not emit a WorkerResponse");
        // The failure reason wins over any finish_reason for the banner.
        state.finish_reason = Some("stop".into());
        let reason = stream_incomplete_reason(&state).expect("must be incomplete");
        assert!(reason.contains("quota exceeded"), "{reason}");
    }

    #[test]
    fn stream_incomplete_reason_priorities() {
        // Clean stop -> None.
        let mut state = StreamState {
            finish_reason: Some("stop".into()),
            ..Default::default()
        };
        assert!(stream_incomplete_reason(&state).is_none());
        // Token cap -> truncation message.
        state.finish_reason = Some("length".into());
        assert!(stream_incomplete_reason(&state).is_some());
        // Server failure outranks the token cap.
        state.fatal = Some("boom".into());
        assert!(stream_incomplete_reason(&state).unwrap().contains("boom"));
    }

    #[test]
    fn handle_stream_event_done_signals_completion() {
        let (tx, _rx) = mpsc::channel();
        let mut state = StreamState::default();
        let done = handle_stream_event(SseEvent::Done, &mut state, &tx, 1);
        assert!(done);
    }

    #[test]
    fn handle_stream_event_finish_records_reason() {
        let (tx, _rx) = mpsc::channel();
        let mut state = StreamState::default();
        handle_stream_event(SseEvent::Finish("length".into()), &mut state, &tx, 1);
        assert_eq!(state.finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn handle_stream_event_usage_recorded_without_side_effects() {
        // A usage chunk carries no visible content and must not send anything.
        let (tx, rx) = mpsc::channel();
        let mut state = StreamState::default();
        let usage = Usage {
            prompt_tokens: 10,
            completion_tokens: 20,
            total_tokens: 30,
        };
        let done = handle_stream_event(SseEvent::Usage(usage), &mut state, &tx, 1);
        assert!(!done);
        assert_eq!(state.usage, Some(usage));
        assert!(rx.try_recv().is_err(), "usage event must not emit a WorkerResponse");
    }

    // -- friendly_status_message tests --

    #[test]
    fn status_message_auth_errors_mention_api_key() {
        for code in [401u16, 403] {
            let msg = friendly_status_message(reqwest::StatusCode::from_u16(code).unwrap());
            assert!(msg.contains("API key"), "HTTP {code}: {msg}");
        }
    }

    #[test]
    fn status_message_not_found_mentions_server_url() {
        let msg = friendly_status_message(reqwest::StatusCode::NOT_FOUND);
        assert!(msg.contains("server URL"));
    }

    #[test]
    fn status_message_payload_errors_suggest_shorter_selection() {
        for code in [400u16, 413] {
            let msg = friendly_status_message(reqwest::StatusCode::from_u16(code).unwrap());
            assert!(msg.contains("shorter selection"), "HTTP {code}: {msg}");
        }
    }

    #[test]
    fn status_message_rate_limit() {
        let msg = friendly_status_message(reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert!(msg.contains("Rate limited"));
    }

    #[test]
    fn status_message_other_4xx_stays_generic_with_code() {
        let msg = friendly_status_message(reqwest::StatusCode::from_u16(418).unwrap());
        assert!(msg.contains("418"), "{msg}");
    }

    #[test]
    fn status_message_5xx_stays_generic() {
        let msg = friendly_status_message(reqwest::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(msg, "Server error. Try again in a moment.");
    }
}
