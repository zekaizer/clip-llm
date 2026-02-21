//! Automated diagnostics system (flywheel).
//!
//! Enabled via `cargo run --features diagnostics`.
//! Automatically captures screenshots + frame data on every state transition.
//! Use `DIAG_MOCK=1` to bypass the LLM server with canned responses.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use eframe::egui;
use serde::Serialize;
use tracing::{debug, info, warn};

use crate::worker::WorkerCommand;
use crate::ProcessMode;

const RING_CAPACITY: usize = 120; // ~2 seconds at 60 fps

// ---------------------------------------------------------------------------
// FrameSnapshot — per-frame diagnostic data
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct FrameSnapshot {
    pub frame: u64,
    pub state: &'static str,
    pub mode: &'static str,
    pub content_size: Option<[f32; 2]>,
    pub desired_size: Option<[f32; 2]>,
    pub viewport_inner_rect: Option<[f32; 4]>,
    pub spawn_position: Option<[f32; 2]>,
    pub user_repositioned: bool,
}

// ---------------------------------------------------------------------------
// TransitionContext — saved at the moment of a state transition
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct TransitionContext {
    from: &'static str,
    to: &'static str,
    frame: u64,
    timestamp: String,
    snapshot: Option<FrameSnapshot>,
    ring_tail: Vec<FrameSnapshot>,
}

// ---------------------------------------------------------------------------
// DiagCollector
// ---------------------------------------------------------------------------

pub struct DiagCollector {
    ring: VecDeque<FrameSnapshot>,
    frame_counter: u64,
    pending_screenshot: bool,
    /// True once we've sent the Screenshot viewport command and are waiting
    /// for the Event::Screenshot callback.
    screenshot_requested: bool,
    dump_counter: u32,
    dump_dir: PathBuf,
    transition_ctx: Option<TransitionContext>,
}

impl DiagCollector {
    pub fn new() -> Self {
        let dump_dir = PathBuf::from("/tmp/clip-llm-diag");
        if let Err(e) = std::fs::create_dir_all(&dump_dir) {
            warn!("diag: failed to create dump dir: {e}");
        } else {
            info!("diag: output dir = {}", dump_dir.display());
        }

        Self {
            ring: VecDeque::with_capacity(RING_CAPACITY),
            frame_counter: 0,
            pending_screenshot: false,
            screenshot_requested: false,
            dump_counter: 0,
            dump_dir,
            transition_ctx: None,
        }
    }

    pub fn frame_counter(&self) -> u64 {
        self.frame_counter
    }

    /// Record per-frame diagnostic data into the ring buffer.
    pub fn record_frame(&mut self, snapshot: FrameSnapshot) {
        if self.ring.len() >= RING_CAPACITY {
            self.ring.pop_front();
        }
        self.ring.push_back(snapshot);
        self.frame_counter += 1;
    }

    /// Called on state transition. Saves context and waits for rendering to settle.
    pub fn on_state_transition(
        &mut self,
        from: &'static str,
        to: &'static str,
    ) {
        debug!("diag: state transition {from} -> {to} at frame {}", self.frame_counter);

        // Save the last N frames as context for this transition.
        let ring_tail: Vec<FrameSnapshot> = self
            .ring
            .iter()
            .rev()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        let snapshot = self.ring.back().cloned();
        let timestamp = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f").to_string();

        self.transition_ctx = Some(TransitionContext {
            from,
            to,
            frame: self.frame_counter,
            timestamp,
            snapshot,
            ring_tail,
        });

        self.pending_screenshot = true;
        self.screenshot_requested = false;
    }

    /// Called every frame after rendering. Checks if the viewport has settled
    /// (content rendered + window resized to match) and then requests screenshot.
    pub fn tick_screenshot(&mut self, ctx: &egui::Context) {
        if !self.pending_screenshot || self.screenshot_requested {
            return;
        }
        if let Some(snap) = self.ring.back() {
            let settled = match (snap.desired_size, snap.viewport_inner_rect) {
                // Content rendered and viewport resized to approximately match.
                (Some(desired), Some(vp)) => {
                    let vp_w = vp[2] - vp[0];
                    let vp_h = vp[3] - vp[1];
                    (vp_w - desired[0]).abs() < 4.0 && (vp_h - desired[1]).abs() < 4.0
                }
                // Hidden state: no content, just capture immediately.
                (None, _) => true,
                _ => false,
            };
            if settled {
                debug!("diag: viewport settled at frame {}, requesting screenshot", self.frame_counter);
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                    egui::UserData::default(),
                ));
                self.screenshot_requested = true;
            }
        }
    }

    /// Called when a screenshot event is received.
    pub fn on_screenshot(&mut self, image: &Arc<egui::ColorImage>) {
        if !self.pending_screenshot {
            return;
        }
        self.pending_screenshot = false;
        self.screenshot_requested = false;
        self.dump_counter += 1;

        let Some(tctx) = self.transition_ctx.take() else {
            return;
        };

        let prefix = format!(
            "{:03}_{}_to_{}",
            self.dump_counter, tctx.from, tctx.to
        );

        // Save PNG.
        let png_path = self.dump_dir.join(format!("{prefix}.png"));
        if let Err(e) = save_color_image_as_png(image, &png_path) {
            warn!("diag: failed to save PNG: {e}");
        } else {
            info!("diag: saved {}", png_path.display());
        }

        // Save JSON sidecar.
        let json_path = self.dump_dir.join(format!("{prefix}.json"));
        match serde_json::to_string_pretty(&tctx) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&json_path, json) {
                    warn!("diag: failed to save JSON: {e}");
                } else {
                    info!("diag: saved {}", json_path.display());
                }
            }
            Err(e) => warn!("diag: JSON serialization failed: {e}"),
        }
    }

    /// Flush any pending transition that didn't receive a screenshot
    /// (e.g. wgpu backend doesn't support it). Called each frame.
    pub fn flush_pending_if_stale(&mut self) {
        // If we've been waiting for more than 30 frames, dump without screenshot.
        if self.pending_screenshot {
            // Check if transition context is old enough (compare frame counter).
            if let Some(ref tctx) = self.transition_ctx {
                if self.frame_counter.saturating_sub(tctx.frame) > 30 {
                    warn!("diag: screenshot timed out, dumping JSON only");
                    self.pending_screenshot = false;
                    self.dump_counter += 1;

                    if let Some(tctx) = self.transition_ctx.take() {
                        let prefix = format!(
                            "{:03}_{}_to_{}",
                            self.dump_counter, tctx.from, tctx.to
                        );
                        let json_path = self.dump_dir.join(format!("{prefix}.json"));
                        if let Ok(json) = serde_json::to_string_pretty(&tctx) {
                            let _ = std::fs::write(&json_path, json);
                            info!("diag: saved {} (no screenshot)", json_path.display());
                        }
                    }
                }
            }
        }
    }
}

fn save_color_image_as_png(
    image: &egui::ColorImage,
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let [w, h] = image.size;
    // Alpha-composite onto a dark background so transparent regions
    // render as visible dark gray instead of white/invisible in PNG.
    let bg: [u8; 3] = [20, 20, 20];
    let rgba: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|c| {
            let a = c.a() as f32 / 255.0;
            let blend = |fg: u8, bg: u8| -> u8 {
                (fg as f32 * a + bg as f32 * (1.0 - a)).round() as u8
            };
            [blend(c.r(), bg[0]), blend(c.g(), bg[1]), blend(c.b(), bg[2]), 255]
        })
        .collect();
    let img = image::RgbaImage::from_raw(w as u32, h as u32, rgba)
        .ok_or("failed to create image buffer")?;
    img.save(path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// DiagScenarioRunner — automatic test scenario injection
// ---------------------------------------------------------------------------

struct Scenario {
    name: &'static str,
    input: &'static str,
    mode: ProcessMode,
    /// Optional mode switch after result arrives (for mode-switch scenario).
    switch_to: Option<ProcessMode>,
}

#[derive(PartialEq)]
enum RunnerPhase {
    /// Waiting for initial startup delay.
    StartupDelay,
    /// Waiting between scenarios (overlay hidden).
    WaitingToInject,
    /// Scenario injected, waiting for Processing -> Result/Error.
    WaitingForResult,
    /// Result received, waiting a bit before hiding.
    WaitingToHide,
    /// Mode switch requested, waiting for new result.
    WaitingForSwitchResult,
    /// All scenarios done.
    Done,
}

pub struct DiagScenarioRunner {
    scenarios: VecDeque<Scenario>,
    phase: RunnerPhase,
    delay_remaining: u32,
}

/// Frames to wait at 60fps.
const STARTUP_DELAY: u32 = 120; // 2 seconds
const BETWEEN_DELAY: u32 = 90; // 1.5 seconds
const RESULT_DISPLAY: u32 = 60; // 1 second to view result before hiding

impl DiagScenarioRunner {
    pub fn new() -> Self {
        let scenarios = VecDeque::from(vec![
            Scenario {
                name: "short_text",
                input: "Hello",
                mode: ProcessMode::Translate,
                switch_to: None,
            },
            Scenario {
                name: "long_text",
                input: "The quick brown fox jumps over the lazy dog. \
                    This is a longer paragraph that should wrap across multiple lines \
                    in the overlay window. It contains enough text to test the scroll \
                    area behavior when the content exceeds the maximum result height.\n\n\
                    Line 3: Testing multi-line content display.\n\
                    Line 4: The overlay should expand to accommodate this text.\n\
                    Line 5: ScrollArea should activate when content is tall enough.\n\
                    Line 6: Checking vertical space allocation after state transition.\n\
                    Line 7: This line tests the Area feedback loop fix.\n\
                    Line 8: Content size should reflect actual rendered height.\n\
                    Line 9: The viewport should resize to match desired_size.\n\
                    Line 10: Final line of the long text scenario.",
                mode: ProcessMode::Translate,
                switch_to: None,
            },
            Scenario {
                name: "mode_switch",
                input: "Testing mode switch from Translate to Correct.",
                mode: ProcessMode::Translate,
                switch_to: Some(ProcessMode::Correct),
            },
        ]);

        Self {
            scenarios,
            phase: RunnerPhase::StartupDelay,
            delay_remaining: STARTUP_DELAY,
        }
    }

    /// Called every frame. Drives the scenario state machine.
    /// Returns an optional action for the OverlayApp to execute.
    pub fn tick(
        &mut self,
        overlay_state: &'static str,
        cmd_tx: &tokio::sync::mpsc::UnboundedSender<WorkerCommand>,
    ) -> ScenarioAction {
        if self.phase == RunnerPhase::Done {
            return ScenarioAction::None;
        }

        match self.phase {
            RunnerPhase::StartupDelay | RunnerPhase::WaitingToInject => {
                if self.delay_remaining > 0 {
                    self.delay_remaining -= 1;
                    return ScenarioAction::None;
                }

                // Inject next scenario.
                if let Some(scenario) = self.scenarios.front() {
                    info!("diag: injecting scenario '{}'", scenario.name);
                    let _ = cmd_tx.send(WorkerCommand::Process {
                        text: scenario.input.to_string(),
                        mode: scenario.mode,
                    });
                    self.phase = RunnerPhase::WaitingForResult;
                    return ScenarioAction::ShowOverlay {
                        mode: scenario.mode,
                    };
                } else {
                    self.phase = RunnerPhase::Done;
                    info!("diag: all scenarios complete");
                }
            }

            RunnerPhase::WaitingForResult => {
                if overlay_state == "Result" || overlay_state == "Error" {
                    // Check if we need a mode switch.
                    if let Some(scenario) = self.scenarios.front() {
                        if let Some(switch_mode) = scenario.switch_to {
                            info!("diag: switching mode to {}", switch_mode.label());
                            self.phase = RunnerPhase::WaitingForSwitchResult;
                            return ScenarioAction::SwitchMode(switch_mode);
                        }
                    }

                    self.delay_remaining = RESULT_DISPLAY;
                    self.phase = RunnerPhase::WaitingToHide;
                }
            }

            RunnerPhase::WaitingForSwitchResult => {
                if overlay_state == "Result" || overlay_state == "Error" {
                    self.delay_remaining = RESULT_DISPLAY;
                    self.phase = RunnerPhase::WaitingToHide;
                }
            }

            RunnerPhase::WaitingToHide => {
                if self.delay_remaining > 0 {
                    self.delay_remaining -= 1;
                    return ScenarioAction::None;
                }

                self.scenarios.pop_front();
                self.delay_remaining = BETWEEN_DELAY;
                self.phase = RunnerPhase::WaitingToInject;
                return ScenarioAction::HideOverlay;
            }

            RunnerPhase::Done => {}
        }

        ScenarioAction::None
    }
}

/// Action the scenario runner requests from the OverlayApp.
pub enum ScenarioAction {
    None,
    ShowOverlay { mode: ProcessMode },
    SwitchMode(ProcessMode),
    HideOverlay,
}

// ---------------------------------------------------------------------------
// Mock response
// ---------------------------------------------------------------------------

/// Generate a mock LLM response based on the input text.
/// Used when `DIAG_MOCK=1` is set.
pub fn mock_response(input: &str) -> String {
    if input.len() < 20 {
        // Short input -> short response.
        format!("[mock] Translated: {input}")
    } else {
        // Long input -> long response that exceeds MAX_RESULT_HEIGHT.
        let mut lines = Vec::new();
        lines.push(format!("[mock] Translated output for: {}...", &input[..20.min(input.len())]));
        for i in 1..=15 {
            lines.push(format!(
                "Line {i}: This is a mock translation line to test scroll behavior and viewport sizing."
            ));
        }
        lines.join("\n")
    }
}
