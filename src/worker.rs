use std::sync::mpsc;
use std::thread;

use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error, info};

use crate::api::client::{LlmClient, SseEvent, SseParser};
use crate::api::response::{strip_think_blocks, ThinkBlockFilter};
use crate::ProcessMode;

pub enum WorkerCommand {
    Process {
        text: String,
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

/// Spawn a worker thread with a tokio runtime for async LLM calls.
/// Returns the thread handle.
///
/// Uses `tokio::sync::mpsc` for the command channel so that `.recv().await`
/// does not block the single-threaded tokio runtime.
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
            let mut cancel_tx: Option<tokio::sync::oneshot::Sender<()>> = None;

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    WorkerCommand::Process { text, mode, request_id } => {
                        // Cancel any in-flight request.
                        if let Some(tx) = cancel_tx.take() {
                            let _ = tx.send(());
                            debug!("cancelled previous in-flight request");
                        }

                        let (c_tx, c_rx) = tokio::sync::oneshot::channel();
                        cancel_tx = Some(c_tx);

                        let llm = llm.clone();
                        let resp_tx = resp_tx.clone();

                        // Mock mode: simulate streaming with canned responses.
                        #[cfg(feature = "diagnostics")]
                        if std::env::var("DIAG_MOCK").is_ok() {
                            info!("worker: mock streaming {} ({} chars)", mode.label(), text.len());
                            tokio::spawn(async move {
                                let mut c_rx = c_rx;
                                match crate::diagnostics::mock_response(&text) {
                                    Ok(mock) => {
                                        let chunks: Vec<&str> =
                                            mock.split_inclusive(char::is_whitespace).collect();
                                        for (i, chunk) in chunks.iter().enumerate() {
                                            if i > 0 {
                                                tokio::select! {
                                                    _ = tokio::time::sleep(std::time::Duration::from_millis(30)) => {}
                                                    _ = &mut c_rx => {
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
                            });
                            continue;
                        }

                        info!("worker: starting stream {} ({} chars)", mode.label(), text.len());

                        tokio::spawn(async move {
                            // 1. Initiate streaming connection.
                            let mut c_rx = c_rx;
                            let resp = tokio::select! {
                                r = llm.complete_stream(&text, mode) => r,
                                _ = &mut c_rx => {
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

                            // 2. Read chunks and parse SSE events.
                            let mut parser = SseParser::new();
                            let mut filter = ThinkBlockFilter::new();
                            let mut full_content = String::new();

                            loop {
                                let chunk = tokio::select! {
                                    c = resp.chunk() => c,
                                    _ = &mut c_rx => {
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
                                                        let _ = resp_tx.send(
                                                            WorkerResponse::StreamDelta {
                                                                text: visible,
                                                                request_id,
                                                            },
                                                        );
                                                    }
                                                }
                                                SseEvent::Done => {
                                                    let result =
                                                        strip_think_blocks(&full_content);
                                                    let r = if result.is_empty() {
                                                        WorkerResponse::Error {
                                                            message: "empty response after \
                                                                      stripping think blocks"
                                                                .into(),
                                                            request_id,
                                                        }
                                                    } else {
                                                        info!(
                                                            "worker: {} stream complete \
                                                             ({} chars)",
                                                            mode.label(),
                                                            result.len()
                                                        );
                                                        WorkerResponse::Complete {
                                                            result,
                                                            request_id,
                                                        }
                                                    };
                                                    let _ = resp_tx.send(r);
                                                    return;
                                                }
                                            }
                                        }
                                    }
                                    Ok(None) => {
                                        // Stream ended without [DONE].
                                        let result = strip_think_blocks(&full_content);
                                        let r = if result.is_empty() {
                                            WorkerResponse::Error {
                                                message: "empty response after stripping \
                                                          think blocks"
                                                    .into(),
                                                request_id,
                                            }
                                        } else {
                                            info!(
                                                "worker: {} stream ended ({} chars)",
                                                mode.label(),
                                                result.len()
                                            );
                                            WorkerResponse::Complete { result, request_id }
                                        };
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
                        });
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
