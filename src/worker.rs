use std::sync::mpsc;
use std::thread;

use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error, info, trace, warn, Instrument};

use crate::api::client::{LlmClient, SseEvent, SseParser};
use crate::api::response::{extract_first_think_content, strip_think_blocks, ThinkBlockFilter};
use crate::{ApiError, ClipboardContent, ProcessMode, RephraseParams, ThinkingMode};

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
    Complete { result: String, think_content: Option<String>, request_id: u64 },
    Error { message: String, request_id: u64 },
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
        ApiError::InitialResponseTimeout(_) => {
            "The server is slow to respond. Try again in a moment.".to_string()
        }
        ApiError::EmptyResponse => {
            "The model returned no visible output. Try switching mode or disabling thinking."
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
    }
}

/// Strip think blocks from raw LLM output and build the appropriate response.
/// Logs completion with the given `label` (e.g. "complete", "stream complete").
fn make_complete_response(
    raw: &str,
    mode: ProcessMode,
    request_id: u64,
    label: &str,
) -> WorkerResponse {
    let think_content = extract_first_think_content(raw);
    let text = strip_think_blocks(raw);
    if text.is_empty() {
        WorkerResponse::Error {
            message: "The model returned no visible output. Try switching mode or disabling thinking."
                .into(),
            request_id,
        }
    } else {
        info!("worker: {} {label} ({} chars)", mode.label(), text.len());
        WorkerResponse::Complete {
            result: text,
            think_content,
            request_id,
        }
    }
}

/// Non-streaming LLM request: single request/response with cancellation support.
async fn run_non_streaming(
    llm: LlmClient,
    task: ProcessTask,
    resp_tx: mpsc::Sender<WorkerResponse>,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let ProcessTask { content, mode, rephrase_params, thinking_mode, request_id } = task;
    let result = tokio::select! {
        r = llm.complete(&content, mode, rephrase_params, thinking_mode) => r,
        _ = &mut cancel_rx => {
            debug!("worker: request cancelled during connect");
            return;
        }
    };
    let r = match result {
        Ok(raw) => make_complete_response(&raw, mode, request_id, "complete"),
        Err(e) => {
            error!("worker: LLM error: {e}");
            WorkerResponse::Error {
                message: friendly_api_error(&e),
                request_id,
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
    let resp = tokio::select! {
        r = llm.complete_stream(&content, mode, rephrase_params, thinking_mode) => r,
        _ = &mut cancel_rx => {
            debug!("worker: request cancelled during connect");
            return;
        }
    };

    let mut resp = match resp {
        Ok(r) => r,
        Err(e) => {
            error!("worker: LLM stream error: {e}");
            let _ = resp_tx.send(WorkerResponse::Error {
                message: friendly_api_error(&e),
                request_id,
            });
            return;
        }
    };

    let mut parser = SseParser::new();
    let mut filter = ThinkBlockFilter::new();
    let mut full_content = String::new();
    let mut think_notified = false;
    let mut finish_reason: Option<String> = None;

    loop {
        let chunk = tokio::select! {
            c = tokio::time::timeout(STREAM_IDLE_TIMEOUT, resp.chunk()) => match c {
                Ok(chunk) => chunk,
                Err(_elapsed) => {
                    warn!(
                        "worker: stream idle for {}s — treating as stalled",
                        STREAM_IDLE_TIMEOUT.as_secs()
                    );
                    let _ = resp_tx.send(WorkerResponse::Error {
                        message: "The server stopped responding mid-reply. Try again.".to_string(),
                        request_id,
                    });
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
                trace!("SSE raw chunk ({} bytes):\n{}", bytes.len(), String::from_utf8_lossy(&bytes));
                for event in parser.feed(&bytes) {
                    match event {
                        SseEvent::Content(token) => {
                            trace!("SSE token: {token:?}");
                            full_content.push_str(&token);
                            let visible = filter.feed(&token);
                            if filter.has_think_content() && !think_notified {
                                think_notified = true;
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
                            trace!("SSE finish_reason: {reason}");
                            finish_reason = Some(reason);
                        }
                        SseEvent::Done => {
                            trace!("SSE stream done, full content ({} chars):\n{full_content}", full_content.len());
                            // "length" means generation hit max_tokens — the
                            // reply is truncated even though the stream ended
                            // normally with [DONE] (#60).
                            if finish_reason.as_deref() == Some("length") {
                                error!(
                                    "worker: generation hit max_tokens after {} chars — treating as truncated",
                                    full_content.len()
                                );
                                let _ = resp_tx.send(WorkerResponse::Error {
                                    message: friendly_api_error(&ApiError::Truncated),
                                    request_id,
                                });
                                return;
                            }
                            let r = make_complete_response(
                                &full_content, mode, request_id, "stream complete",
                            );
                            let _ = resp_tx.send(r);
                            return;
                        }
                    }
                }
            }
            Ok(None) => {
                // The Done arm returns, so reaching here means the connection
                // closed before `data: [DONE]` arrived — the reply is truncated
                // (proxy timeout, server restart, network reset). Surface an
                // error instead of completing with partial content (#60).
                error!(
                    "worker: stream closed without [DONE] after {} chars — treating as truncated",
                    full_content.len()
                );
                let _ = resp_tx.send(WorkerResponse::Error {
                    message: "The connection closed before the reply finished. Try again."
                        .to_string(),
                    request_id,
                });
                return;
            }
            Err(e) => {
                error!("worker: stream chunk error: {e}");
                let _ = resp_tx.send(WorkerResponse::Error {
                    message: friendly_reqwest_error(&e),
                    request_id,
                });
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
            });
        }
        Err(msg) => {
            info!("worker: mock error: {msg}");
            let _ = resp_tx.send(WorkerResponse::Error {
                message: msg,
                request_id,
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

    let llm = llm.clone();
    let resp_tx = resp_tx.clone();

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
            WorkerResponse::Error { message, request_id } => {
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
            WorkerResponse::Complete { .. } => "Complete",
            WorkerResponse::Error { .. } => "Error",
            WorkerResponse::ProbeComplete { .. } => "ProbeComplete",
        }
    }

    #[test]
    fn make_complete_response_plain_text() {
        let r = make_complete_response("hello world", ProcessMode::Translate, 1, "complete");
        let text = assert_complete(&r, 1);
        assert_eq!(text, "hello world");
    }

    #[test]
    fn make_complete_response_strips_think_blocks() {
        let raw = "<think>internal</think>visible output";
        let r = make_complete_response(raw, ProcessMode::Rephrase, 2, "stream complete");
        let text = assert_complete(&r, 2);
        assert_eq!(text, "visible output");
    }

    #[test]
    fn make_complete_response_empty_after_strip() {
        let raw = "<think>only thinking</think>";
        let r = make_complete_response(raw, ProcessMode::Summarize, 3, "complete");
        let msg = assert_error(&r, 3);
        assert!(msg.contains("no visible output"));
    }

    #[test]
    fn make_complete_response_empty_input() {
        let r = make_complete_response("", ProcessMode::Translate, 4, "complete");
        let msg = assert_error(&r, 4);
        assert!(msg.contains("no visible output"));
    }

    #[test]
    fn make_complete_response_whitespace_only_after_strip() {
        let raw = "<think>x</think>   \n  ";
        let r = make_complete_response(raw, ProcessMode::Translate, 5, "complete");
        // strip_think_blocks trims whitespace; empty after trim → error.
        let msg = assert_error(&r, 5);
        assert!(msg.contains("no visible output"));
    }

    #[test]
    fn make_complete_response_preserves_request_id() {
        let r = make_complete_response("ok", ProcessMode::Translate, 42, "test");
        assert_complete(&r, 42);
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
