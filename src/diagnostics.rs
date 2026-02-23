//! Automated diagnostics system (flywheel).
//!
//! Enabled via `cargo run --features diagnostics`.
//! Automatically captures screenshots + frame data on every state transition.
//! Use `DIAG_MOCK=1` to bypass the LLM server with canned responses.

// DiagCollector::new() and DiagScenarioRunner::new() have side effects
// (dir creation, scenario setup) that make Default impls misleading.
#![allow(clippy::new_without_default)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::Serialize;
use tracing::{debug, info, warn};

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

/// Extra frames to wait after viewport settles before capturing.
/// macOS compositor applies a fade-in when a window becomes visible;
/// this delay lets the window reach full opacity.
const POST_SETTLE_DELAY: u64 = 10;

pub struct DiagCollector {
    ring: VecDeque<FrameSnapshot>,
    frame_counter: u64,
    pending_screenshot: bool,
    /// True once we've sent the Screenshot viewport command and are waiting
    /// for the Event::Screenshot callback.
    screenshot_requested: bool,
    /// Frame at which the viewport first settled (size matched desired).
    settled_at_frame: Option<u64>,
    dump_counter: u32,
    dump_dir: PathBuf,
    transition_ctx: Option<TransitionContext>,
}

impl DiagCollector {
    pub fn new() -> Self {
        // Output under target/diagnostics/ — gitignored and project-local.
        let dump_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("diagnostics");
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
            settled_at_frame: None,
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
        self.settled_at_frame = None;
    }

    /// Called every frame after rendering. Checks if the viewport has settled
    /// (content rendered + window resized to match), then waits for macOS
    /// fade-in to complete before requesting the screenshot.
    pub fn tick_screenshot(&mut self, ctx: &egui::Context) {
        if !self.pending_screenshot || self.screenshot_requested {
            return;
        }
        if let Some(snap) = self.ring.back() {
            let settled = match (snap.desired_size, snap.viewport_inner_rect) {
                (Some(desired), Some(vp)) => {
                    let vp_w = vp[2] - vp[0];
                    let vp_h = vp[3] - vp[1];
                    (vp_w - desired[0]).abs() < 4.0 && (vp_h - desired[1]).abs() < 4.0
                }
                (None, _) => true,
                _ => false,
            };

            if settled {
                let settled_frame = *self.settled_at_frame.get_or_insert(self.frame_counter);
                if self.frame_counter >= settled_frame + POST_SETTLE_DELAY {
                    debug!(
                        "diag: requesting screenshot at frame {} (settled at {})",
                        self.frame_counter, settled_frame
                    );
                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                        egui::UserData::default(),
                    ));
                    self.screenshot_requested = true;
                }
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
    // egui::Color32 uses premultiplied alpha — r()/g()/b() already have
    // alpha baked in. Composite: out = src_premul + bg * (1 - src_a).
    let rgba: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|c| {
            let a = c.a() as f32 / 255.0;
            let blend = |fg_premul: u8, bg: u8| -> u8 {
                (fg_premul as f32 + bg as f32 * (1.0 - a))
                    .round()
                    .min(255.0) as u8
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

#[derive(Debug, PartialEq)]
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
    /// All scenarios done, waiting before quit.
    Finishing,
    /// Quit signal sent.
    Done,
}

pub struct DiagScenarioRunner {
    scenarios: VecDeque<Scenario>,
    phase: RunnerPhase,
    /// Deadline for the current delay (time-based, independent of frame rate).
    delay_until: Instant,
}

const STARTUP_DELAY: Duration = Duration::from_secs(2);
const BETWEEN_DELAY: Duration = Duration::from_millis(1500);
const RESULT_DISPLAY: Duration = Duration::from_secs(1);
const QUIT_DELAY: Duration = Duration::from_secs(1);

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
                input: "Testing mode switch from Translate to Rephrase.",
                mode: ProcessMode::Translate,
                switch_to: Some(ProcessMode::Rephrase),
            },
            Scenario {
                name: "error_display",
                input: "__ERROR__",
                mode: ProcessMode::Translate,
                switch_to: None,
            },
            Scenario {
                name: "korean_text",
                input: "안녕하세요. 이것은 한국어 텍스트 렌더링을 테스트하기 위한 시나리오입니다.",
                mode: ProcessMode::Translate,
                switch_to: None,
            },
            Scenario {
                name: "rephrase_mode",
                input: "This sentense has speling erors that need correcting.",
                mode: ProcessMode::Rephrase,
                switch_to: None,
            },
            Scenario {
                name: "summarize_mode",
                input: "Rust is a systems programming language focused on safety, speed, and \
                    concurrency. It achieves memory safety without garbage collection through \
                    its ownership system with three key rules: each value has exactly one owner, \
                    ownership can be transferred (moved) but not implicitly copied, and when the \
                    owner goes out of scope the value is dropped. The borrow checker enforces \
                    these rules at compile time, preventing data races and use-after-free bugs \
                    without any runtime overhead. Rust also provides fearless concurrency through \
                    its type system, ensuring thread safety at compile time rather than relying \
                    on runtime checks or locks.",
                mode: ProcessMode::Summarize,
                switch_to: None,
            },
            Scenario {
                name: "long_single_line",
                input: "A",
                mode: ProcessMode::Translate,
                switch_to: None,
            },
        ]);

        Self {
            scenarios,
            phase: RunnerPhase::StartupDelay,
            delay_until: Instant::now() + STARTUP_DELAY,
        }
    }

    /// Called every frame. Drives the scenario state machine.
    /// Returns an action for the OverlayApp adapter to execute via StateMachine.
    pub fn tick(&mut self, overlay_state: &str) -> ScenarioAction {
        if self.phase == RunnerPhase::Done {
            return ScenarioAction::None;
        }

        match self.phase {
            RunnerPhase::StartupDelay | RunnerPhase::WaitingToInject => {
                if Instant::now() < self.delay_until {
                    return ScenarioAction::None;
                }

                // Inject next scenario.
                if let Some(scenario) = self.scenarios.front() {
                    info!("diag: injecting scenario '{}'", scenario.name);
                    self.phase = RunnerPhase::WaitingForResult;
                    return ScenarioAction::ShowOverlay {
                        mode: scenario.mode,
                        text: scenario.input.to_string(),
                    };
                } else {
                    self.phase = RunnerPhase::Finishing;
                    self.delay_until = Instant::now() + QUIT_DELAY;
                    info!("diag: all scenarios complete, finishing");
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

                    self.delay_until = Instant::now() + RESULT_DISPLAY;
                    self.phase = RunnerPhase::WaitingToHide;
                }
            }

            RunnerPhase::WaitingForSwitchResult => {
                if overlay_state == "Result" || overlay_state == "Error" {
                    self.delay_until = Instant::now() + RESULT_DISPLAY;
                    self.phase = RunnerPhase::WaitingToHide;
                }
            }

            RunnerPhase::WaitingToHide => {
                if Instant::now() < self.delay_until {
                    return ScenarioAction::None;
                }

                self.scenarios.pop_front();
                self.delay_until = Instant::now() + BETWEEN_DELAY;
                self.phase = RunnerPhase::WaitingToInject;
                return ScenarioAction::HideOverlay;
            }

            RunnerPhase::Finishing => {
                if Instant::now() < self.delay_until {
                    return ScenarioAction::None;
                }
                self.phase = RunnerPhase::Done;
                info!("diag: quitting");
                return ScenarioAction::Quit;
            }

            RunnerPhase::Done => {}
        }

        ScenarioAction::None
    }
}

/// Action the scenario runner requests from the OverlayApp.
#[derive(Debug)]
pub enum ScenarioAction {
    None,
    ShowOverlay {
        mode: ProcessMode,
        text: String,
    },
    SwitchMode(ProcessMode),
    HideOverlay,
    /// All scenarios complete — app should exit.
    Quit,
}

// ---------------------------------------------------------------------------
// Scenario runner thread
// ---------------------------------------------------------------------------

/// Run the scenario runner on the current thread (blocking).
///
/// Observes overlay state via `state_rx` and sends `ScenarioAction` back to
/// the UI thread via `action_tx`. When a scenario needs the window visible
/// (ShowOverlay), calls `pre_show` first — on Windows this ensures `WM_PAINT`
/// is delivered so eframe `update()` fires.
///
/// Timing is `Instant`-based (frame-independent). The loop polls `state_rx`
/// with 100ms timeout to drive `tick()` periodically even without state changes.
pub fn run_scenario_thread(
    state_rx: mpsc::Receiver<&'static str>,
    action_tx: mpsc::Sender<ScenarioAction>,
    ctx: egui::Context,
    pre_show: Box<dyn Fn() + Send>,
) {
    let mut runner = DiagScenarioRunner::new();
    let mut current_state: &str = "Hidden";

    info!("diagnostics scenario thread started");

    loop {
        // Poll for state updates, timeout for periodic ticking.
        match state_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(state) => current_state = state,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        let action = runner.tick(current_state);
        match action {
            ScenarioAction::None => continue,
            ScenarioAction::Quit => {
                let _ = action_tx.send(action);
                ctx.request_repaint();
                break;
            }
            ScenarioAction::ShowOverlay { .. } => {
                pre_show();
                let _ = action_tx.send(action);
                ctx.request_repaint();
            }
            action => {
                let _ = action_tx.send(action);
                ctx.request_repaint();
            }
        }
    }

    info!("diagnostics scenario thread exiting");
}

// ---------------------------------------------------------------------------
// Mock response
// ---------------------------------------------------------------------------

/// Generate a mock LLM response based on the input text.
/// Used when `DIAG_MOCK=1` is set.
/// Returns `Err` for inputs starting with `__ERROR__`.
pub fn mock_response(input: &str) -> Result<String, String> {
    if input.starts_with("__ERROR__") {
        return Err("Connection refused: vLLM server not reachable at localhost:8000".into());
    }

    let response = if input == "A" {
        // Single-line but long enough to test text wrapping.
        "[mock] This is a single long line that should wrap within the overlay width to verify \
         that text wrapping works correctly when the result is just one continuous sentence \
         without any explicit line breaks in the content."
            .to_string()
    } else if input.len() < 20 {
        format!("[mock] Translated: {input}")
    } else {
        let mut lines = Vec::new();
        lines.push(format!(
            "[mock] Translated output for: {}...",
            &input[..20.min(input.len())]
        ));
        for i in 1..=15 {
            lines.push(format!(
                "Line {i}: This is a mock translation line to test scroll behavior and viewport sizing."
            ));
        }
        lines.join("\n")
    };
    Ok(response)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Expire the current delay so the next tick proceeds immediately.
    fn expire_delay(runner: &mut DiagScenarioRunner) {
        runner.delay_until = Instant::now() - Duration::from_secs(1);
    }

    #[test]
    fn startup_delay_returns_none() {
        let runner = DiagScenarioRunner::new();
        // Delay is in the future, so tick should return None.
        assert!(matches!(runner.phase, RunnerPhase::StartupDelay));
    }

    #[test]
    fn first_scenario_after_delay() {
        let mut runner = DiagScenarioRunner::new();
        expire_delay(&mut runner);

        match runner.tick("Hidden") {
            ScenarioAction::ShowOverlay { mode, text } => {
                assert_eq!(mode, ProcessMode::Translate);
                assert_eq!(text, "Hello");
            }
            other => panic!("expected ShowOverlay, got {other:?}"),
        }
    }

    #[test]
    fn result_triggers_wait_to_hide() {
        let mut runner = DiagScenarioRunner::new();
        expire_delay(&mut runner);

        // First scenario injection.
        let action = runner.tick("Hidden");
        assert!(matches!(action, ScenarioAction::ShowOverlay { .. }));

        // Simulate worker completing — overlay shows "Result".
        // Runner should transition to WaitingToHide phase.
        assert!(matches!(runner.tick("Result"), ScenarioAction::None));
        assert_eq!(runner.phase, RunnerPhase::WaitingToHide);
    }

    #[test]
    fn mode_switch_scenario_flow() {
        let mut runner = DiagScenarioRunner::new();
        expire_delay(&mut runner);

        // Consume first two scenarios (short_text, long_text).
        // Scenario 1: short_text
        runner.tick("Hidden"); // ShowOverlay
        runner.tick("Result"); // -> WaitingToHide
        expire_delay(&mut runner);
        runner.tick("Result"); // HideOverlay
        expire_delay(&mut runner);

        // Scenario 2: long_text
        runner.tick("Hidden"); // ShowOverlay
        runner.tick("Result"); // -> WaitingToHide
        expire_delay(&mut runner);
        runner.tick("Result"); // HideOverlay
        expire_delay(&mut runner);

        // Scenario 3: mode_switch — should trigger SwitchMode after result.
        let action = runner.tick("Hidden");
        assert!(matches!(action, ScenarioAction::ShowOverlay { .. }));

        // Result arrives — runner should return SwitchMode.
        match runner.tick("Result") {
            ScenarioAction::SwitchMode(mode) => {
                assert_eq!(mode, ProcessMode::Rephrase);
            }
            other => panic!("expected SwitchMode, got {other:?}"),
        }

        // After switch, wait for new result.
        assert!(matches!(runner.tick("Processing"), ScenarioAction::None));

        // New result arrives.
        assert!(matches!(runner.tick("Result"), ScenarioAction::None));
        assert_eq!(runner.phase, RunnerPhase::WaitingToHide);
    }

    #[test]
    fn hide_after_display_delay() {
        let mut runner = DiagScenarioRunner::new();
        expire_delay(&mut runner);

        runner.tick("Hidden"); // ShowOverlay
        runner.tick("Result"); // -> WaitingToHide

        // Expire display delay.
        expire_delay(&mut runner);

        // Next tick should hide.
        assert!(matches!(runner.tick("Result"), ScenarioAction::HideOverlay));
    }

    #[test]
    fn quit_after_all_scenarios() {
        let mut runner = DiagScenarioRunner::new();
        let scenario_count = runner.scenarios.len();
        expire_delay(&mut runner);

        // Scenarios: short_text, long_text, mode_switch, error_display,
        //            korean_text, correct_mode, summarize_mode, long_single_line
        // mode_switch (index 2) returns SwitchMode on first Result.
        // error_display (index 3) produces Error state.
        for i in 0..scenario_count {
            let action = runner.tick("Hidden"); // ShowOverlay
            assert!(matches!(action, ScenarioAction::ShowOverlay { .. }),
                "scenario {i}: expected ShowOverlay, got {action:?}");

            // error_display scenario produces Error, rest produce Result.
            let result_state = if i == 3 { "Error" } else { "Result" };

            let action = runner.tick(result_state);
            if matches!(action, ScenarioAction::SwitchMode(_)) {
                // mode_switch: wait for re-processing, then new result.
                runner.tick("Processing");
                runner.tick("Result");
            }
            // Expire display delay and hide.
            expire_delay(&mut runner);
            runner.tick(result_state); // HideOverlay
            expire_delay(&mut runner);
        }

        // One more tick to transition from WaitingToInject (empty) to Finishing.
        assert!(matches!(runner.tick("Hidden"), ScenarioAction::None));
        assert_eq!(runner.phase, RunnerPhase::Finishing);

        expire_delay(&mut runner);
        assert!(matches!(runner.tick("Hidden"), ScenarioAction::Quit));
    }

    #[test]
    fn done_returns_none() {
        let mut runner = DiagScenarioRunner::new();
        runner.phase = RunnerPhase::Done;

        assert!(matches!(runner.tick("Hidden"), ScenarioAction::None));
        assert!(matches!(runner.tick("Result"), ScenarioAction::None));
    }
}
