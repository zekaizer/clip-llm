use std::sync::mpsc;
use std::thread;

use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error, info};

use crate::api::client::{LlmClient, SseEvent, SseParser};
use crate::api::response::{extract_first_think_content, strip_think_blocks, ThinkBlockFilter};
use crate::{ClipboardContent, ProcessMode, RephraseParams, ThinkingMode};

/// Runtime configuration resolved from environment variables once at worker startup.
#[derive(Copy, Clone)]
struct WorkerConfig {
    /// Use streaming SSE API. Disabled by setting `CLIP_LLM_NO_STREAM`.
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
    /// One-shot: thinking control capability from probe (sent once at startup).
    ThinkingProbeComplete { supported: bool },
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
            message: "empty response after stripping think blocks".into(),
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
                message: e.to_string(),
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
                message: e.to_string(),
                request_id,
            });
            return;
        }
    };

    let mut parser = SseParser::new();
    let mut filter = ThinkBlockFilter::new();
    let mut full_content = String::new();
    let mut think_notified = false;

    loop {
        let chunk = tokio::select! {
            c = resp.chunk() => c,
            _ = &mut cancel_rx => {
                debug!("worker: request cancelled during streaming");
                return;
            }
        };

        match chunk {
            Ok(Some(bytes)) => {
                for event in parser.feed(&bytes) {
                    match event {
                        SseEvent::Content(token) => {
                            full_content.push_str(&token);
                            let visible = filter.feed(&token);
                            if filter.is_thinking() && !think_notified {
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
                        SseEvent::Done => {
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
                let r = make_complete_response(
                    &full_content, mode, request_id, "stream ended",
                );
                let _ = resp_tx.send(r);
                return;
            }
            Err(e) => {
                error!("worker: stream chunk error: {e}");
                let _ = resp_tx.send(WorkerResponse::Error {
                    message: e.to_string(),
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

    // Mock mode: simulate streaming with canned responses.
    #[cfg(feature = "diagnostics")]
    if config.use_mock {
        let mode = task.mode;
        let request_id = task.request_id;
        let mock_text = task.content.text.unwrap_or_default();
        info!("worker: mock streaming {} ({} chars)", mode.label(), mock_text.len());
        tokio::spawn(run_mock_streaming(mock_text, mode, request_id, resp_tx, c_rx));
        return;
    }

    let text_len = task.content.text.as_ref().map_or(0, |t| t.len());
    let img_count = task.content.images.len();

    if config.streaming {
        info!(
            "worker: starting stream {} ({} chars, {} images)",
            task.mode.label(), text_len, img_count,
        );
        tokio::spawn(run_streaming(llm, task, resp_tx, c_rx));
    } else {
        info!(
            "worker: starting {} ({} chars, {} images, no-stream)",
            task.mode.label(), text_len, img_count,
        );
        tokio::spawn(run_non_streaming(llm, task, resp_tx, c_rx));
    }
}

/// Spawn a worker thread with a tokio runtime for async LLM calls.
/// Returns the thread handle.
///
/// Uses `tokio::sync::mpsc` for the command channel so that `.recv().await`
/// does not block the single-threaded tokio runtime.
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
            WorkerResponse::ThinkingProbeComplete { .. } => "ThinkingProbeComplete",
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
        assert!(msg.contains("empty response"));
    }

    #[test]
    fn make_complete_response_empty_input() {
        let r = make_complete_response("", ProcessMode::Translate, 4, "complete");
        let msg = assert_error(&r, 4);
        assert!(msg.contains("empty response"));
    }

    #[test]
    fn make_complete_response_whitespace_only_after_strip() {
        let raw = "<think>x</think>   \n  ";
        let r = make_complete_response(raw, ProcessMode::Translate, 5, "complete");
        // strip_think_blocks trims whitespace; empty after trim → error.
        let msg = assert_error(&r, 5);
        assert!(msg.contains("empty response"));
    }

    #[test]
    fn make_complete_response_preserves_request_id() {
        let r = make_complete_response("ok", ProcessMode::Translate, 42, "test");
        assert_complete(&r, 42);
    }
}

pub fn spawn_worker(
    mut cmd_rx: tokio_mpsc::UnboundedReceiver<WorkerCommand>,
    resp_tx: mpsc::Sender<WorkerResponse>,
    llm: LlmClient,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime");

        // Read env vars once at thread start — no async context needed.
        let config = WorkerConfig {
            streaming: std::env::var("CLIP_LLM_NO_STREAM").is_err(),
            #[cfg(feature = "diagnostics")]
            use_mock: std::env::var("DIAG_MOCK").is_ok(),
        };

        rt.block_on(async move {
            // Probe vision and thinking support eagerly so they don't delay the first request.
            llm.probe_vision().await;
            let thinking_method = llm.probe_thinking().await;
            let supported =
                thinking_method != crate::api::client::ThinkingControlMethod::Unsupported;
            let _ = resp_tx.send(WorkerResponse::ThinkingProbeComplete { supported });

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
