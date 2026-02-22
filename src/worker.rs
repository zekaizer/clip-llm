use std::sync::mpsc;
use std::thread;

use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error, info};

use crate::api::client::{LlmClient, SseEvent, SseParser};
use crate::api::response::{strip_think_blocks, ThinkBlockFilter};
use crate::{ClipboardContent, ProcessMode};

pub enum WorkerCommand {
    Process {
        content: ClipboardContent,
        mode: ProcessMode,
        request_id: u64,
    },
    Cancel,
}

pub enum WorkerResponse {
    StreamDelta { text: String, request_id: u64 },
    Complete { result: String, request_id: u64 },
    Error { message: String, request_id: u64 },
}

/// Strip think blocks from raw LLM output and build the appropriate response.
/// Logs completion with the given `label` (e.g. "complete", "stream complete").
fn make_complete_response(
    raw: &str,
    mode: ProcessMode,
    request_id: u64,
    label: &str,
) -> WorkerResponse {
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
            request_id,
        }
    }
}

/// Non-streaming LLM request: single request/response with cancellation support.
async fn run_non_streaming(
    llm: LlmClient,
    content: ClipboardContent,
    mode: ProcessMode,
    request_id: u64,
    resp_tx: mpsc::Sender<WorkerResponse>,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let result = tokio::select! {
        r = llm.complete(&content, mode) => r,
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
    content: ClipboardContent,
    mode: ProcessMode,
    request_id: u64,
    resp_tx: mpsc::Sender<WorkerResponse>,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let resp = tokio::select! {
        r = llm.complete_stream(&content, mode) => r,
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
    content: ClipboardContent,
    mode: ProcessMode,
    request_id: u64,
    llm: &LlmClient,
    resp_tx: &mpsc::Sender<WorkerResponse>,
    cancel_tx: &mut Option<tokio::sync::oneshot::Sender<()>>,
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
    let text_for_log = content.text.as_deref().unwrap_or("");

    // Mock mode: simulate streaming with canned responses.
    #[cfg(feature = "diagnostics")]
    if std::env::var("DIAG_MOCK").is_ok() {
        let mock_text = text_for_log.to_owned();
        info!("worker: mock streaming {} ({} chars)", mode.label(), mock_text.len());
        tokio::spawn(run_mock_streaming(mock_text, mode, request_id, resp_tx, c_rx));
        return;
    }

    let streaming = std::env::var("CLIP_LLM_NO_STREAM").is_err();

    if streaming {
        info!(
            "worker: starting stream {} ({} chars, {} images)",
            mode.label(), text_for_log.len(), content.images.len(),
        );
        tokio::spawn(run_streaming(llm, content, mode, request_id, resp_tx, c_rx));
    } else {
        info!(
            "worker: starting {} ({} chars, {} images, no-stream)",
            mode.label(), text_for_log.len(), content.images.len(),
        );
        tokio::spawn(run_non_streaming(llm, content, mode, request_id, resp_tx, c_rx));
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
            WorkerResponse::Complete { result, request_id } => {
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
            WorkerResponse::Complete { .. } => "Complete",
            WorkerResponse::Error { .. } => "Error",
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
        let r = make_complete_response(raw, ProcessMode::Correct, 2, "stream complete");
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

        rt.block_on(async move {
            // Probe vision support eagerly so it doesn't delay the first user request.
            llm.probe_vision().await;

            let mut cancel_tx: Option<tokio::sync::oneshot::Sender<()>> = None;

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    WorkerCommand::Process { content, mode, request_id } => {
                        dispatch_process(
                            content, mode, request_id,
                            &llm, &resp_tx, &mut cancel_tx,
                        );
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
