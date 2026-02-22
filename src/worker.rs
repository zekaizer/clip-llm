use std::sync::mpsc;
use std::thread;

use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error, info};

use std::env;

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
            // Probe vision support eagerly so it doesn't delay the first user request.
            llm.probe_vision().await;

            let mut cancel_tx: Option<tokio::sync::oneshot::Sender<()>> = None;

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    WorkerCommand::Process { content, mode, request_id } => {
                        // Cancel any in-flight request.
                        if let Some(tx) = cancel_tx.take() {
                            let _ = tx.send(());
                            debug!("cancelled previous in-flight request");
                        }

                        let (c_tx, c_rx) = tokio::sync::oneshot::channel();
                        cancel_tx = Some(c_tx);

                        let llm = llm.clone();
                        let resp_tx = resp_tx.clone();

                        let text_for_log = content.text.as_deref().unwrap_or("");

                        // Mock mode: simulate streaming with canned responses.
                        #[cfg(feature = "diagnostics")]
                        if std::env::var("DIAG_MOCK").is_ok() {
                            let mock_text = text_for_log.to_owned();
                            info!("worker: mock streaming {} ({} chars)", mode.label(), mock_text.len());
                            tokio::spawn(async move {
                                let mut c_rx = c_rx;
                                match crate::diagnostics::mock_response(&mock_text) {
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

                        let streaming = env::var("CLIP_LLM_NO_STREAM").is_err();

                        if streaming {
                            info!(
                                "worker: starting stream {} ({} chars, {} images)",
                                mode.label(),
                                text_for_log.len(),
                                content.images.len(),
                            );
                        } else {
                            info!(
                                "worker: starting {} ({} chars, {} images, no-stream)",
                                mode.label(),
                                text_for_log.len(),
                                content.images.len(),
                            );
                        }

                        tokio::spawn(async move {
                            let mut c_rx = c_rx;

                            if !streaming {
                                // Non-streaming: single request/response.
                                let result = tokio::select! {
                                    r = llm.complete(&content, mode) => r,
                                    _ = &mut c_rx => {
                                        debug!("worker: request cancelled during connect");
                                        return;
                                    }
                                };
                                let r = match result {
                                    Ok(raw) => {
                                        let text = strip_think_blocks(&raw);
                                        if text.is_empty() {
                                            WorkerResponse::Error {
                                                message: "empty response after stripping think blocks".into(),
                                                request_id,
                                            }
                                        } else {
                                            info!("worker: {} complete ({} chars)", mode.label(), text.len());
                                            WorkerResponse::Complete { result: text, request_id }
                                        }
                                    }
                                    Err(e) => {
                                        error!("worker: LLM error: {e}");
                                        WorkerResponse::Error { message: e.to_string(), request_id }
                                    }
                                };
                                let _ = resp_tx.send(r);
                                return;
                            }

                            // Streaming path.
                            let resp = tokio::select! {
                                r = llm.complete_stream(&content, mode) => r,
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
