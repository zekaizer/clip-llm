use std::sync::mpsc;
use std::thread;

use tokio::sync::mpsc as tokio_mpsc;
use tracing::{debug, error, info};

use crate::api::client::LlmClient;
use crate::api::response::strip_think_blocks;
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

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    WorkerCommand::Process { text, mode, request_id } => {
                        // Cancel any in-flight request.
                        if let Some(tx) = cancel_tx.take() {
                            let _ = tx.send(());
                            debug!("cancelled previous in-flight request");
                        }

                        // Mock mode: return canned responses without LLM call.
                        #[cfg(feature = "diagnostics")]
                        if std::env::var("DIAG_MOCK").is_ok() {
                            let resp = match crate::diagnostics::mock_response(&text) {
                                Ok(mock) => {
                                    info!("worker: mock {} ({} chars)", mode.label(), mock.len());
                                    WorkerResponse::Complete { result: mock, request_id }
                                }
                                Err(msg) => {
                                    info!("worker: mock error: {msg}");
                                    WorkerResponse::Error { message: msg, request_id }
                                }
                            };
                            let _ = resp_tx.send(resp);
                            continue;
                        }

                        let (c_tx, c_rx) = tokio::sync::oneshot::channel();
                        cancel_tx = Some(c_tx);

                        let llm = llm.clone();
                        let resp_tx = resp_tx.clone();

                        info!("worker: starting {} ({} chars)", mode.label(), text.len());

                        tokio::spawn(async move {
                            let result = tokio::select! {
                                r = llm.complete(&text, mode) => r,
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
                                            request_id,
                                        }
                                    } else {
                                        info!(
                                            "worker: {} complete ({} chars)",
                                            mode.label(),
                                            stripped.len()
                                        );
                                        WorkerResponse::Complete { result: stripped, request_id }
                                    }
                                }
                                Err(e) => {
                                    error!("worker: LLM error: {e}");
                                    WorkerResponse::Error {
                                        message: e.to_string(),
                                        request_id,
                                    }
                                }
                            };

                            let _ = resp_tx.send(response);
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
