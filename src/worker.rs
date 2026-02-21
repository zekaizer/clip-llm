use std::sync::mpsc;
use std::thread;

use tracing::{debug, error, info};

use crate::api::client::LlmClient;
use crate::api::response::strip_think_blocks;

pub enum WorkerCommand {
    Translate { text: String },
    Cancel,
}

pub enum WorkerResponse {
    Complete { result: String },
    Error { message: String },
}

/// Spawn a worker thread with a tokio runtime for async LLM calls.
/// Returns the thread handle.
pub fn spawn_worker(
    cmd_rx: mpsc::Receiver<WorkerCommand>,
    resp_tx: mpsc::SyncSender<WorkerResponse>,
    llm: LlmClient,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime");

        rt.block_on(async move {
            let mut cancel_tx: Option<tokio::sync::oneshot::Sender<()>> = None;

            loop {
                match cmd_rx.recv() {
                    Ok(WorkerCommand::Translate { text }) => {
                        // Cancel any in-flight request.
                        if let Some(tx) = cancel_tx.take() {
                            let _ = tx.send(());
                            debug!("cancelled previous in-flight request");
                        }

                        let (c_tx, c_rx) = tokio::sync::oneshot::channel();
                        cancel_tx = Some(c_tx);

                        let llm = llm.clone();
                        let resp_tx = resp_tx.clone();

                        info!("worker: starting translation ({} chars)", text.len());

                        tokio::spawn(async move {
                            let result = tokio::select! {
                                r = llm.complete(&text) => r,
                                _ = c_rx => {
                                    debug!("worker: request cancelled");
                                    return;
                                }
                            };

                            let response = match result {
                                Ok(raw) => {
                                    let stripped = strip_think_blocks(&raw);
                                    if stripped.is_empty() {
                                        WorkerResponse::Error {
                                            message: "empty response after stripping think blocks"
                                                .into(),
                                        }
                                    } else {
                                        info!(
                                            "worker: translation complete ({} chars)",
                                            stripped.len()
                                        );
                                        WorkerResponse::Complete { result: stripped }
                                    }
                                }
                                Err(e) => {
                                    error!("worker: LLM error: {e}");
                                    WorkerResponse::Error {
                                        message: e.to_string(),
                                    }
                                }
                            };

                            let _ = resp_tx.send(response);
                        });
                    }
                    Ok(WorkerCommand::Cancel) => {
                        if let Some(tx) = cancel_tx.take() {
                            let _ = tx.send(());
                            info!("worker: cancelled by user");
                        }
                    }
                    Err(_) => {
                        info!("worker: command channel closed, exiting");
                        break;
                    }
                }
            }
        });
    })
}
