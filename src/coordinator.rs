use std::sync::mpsc;
use std::time::Duration;

use eframe::egui;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use tracing::{debug, info};

use crate::hotkey::{HotkeyDetector, TapAction, TapEvent};
use crate::platform::ModifierState;

/// Coordinator gesture phase. After a trigger resolves while the modifiers stay
/// held, additional C taps advance the mode-cycle preview until the modifiers
/// are released (commit).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No gesture in progress.
    Idle,
    /// A first tap was seen; waiting to confirm single vs double tap.
    AwaitingTrigger,
    /// Trigger committed and modifiers still held; C taps cycle the mode.
    Cycling { is_double_tap: bool },
}

/// Run the coordinator loop on the current thread (blocking).
///
/// Detects single/double-tap hotkey patterns and forwards [`TapEvent`] to the
/// UI thread via `tap_tx`. This is 100% common code — platform-specific window
/// show logic is injected via the `pre_show` callback.
///
/// `mouse_pos_fn` captures the mouse position at first key press so the overlay
/// appears where the user triggered the hotkey, not where the cursor is after
/// the double-tap timeout or copy simulation delay.
///
/// The loop is event-driven:
/// - Idle: blocks on `recv()` (zero CPU).
/// - During the double-tap window (`double_tap_timeout`, default 500ms): polls
///   with `recv_timeout(50ms)`.
pub fn run(
    hotkey_rx: mpsc::Receiver<GlobalHotKeyEvent>,
    tap_tx: mpsc::Sender<TapEvent>,
    ctx: egui::Context,
    pre_show: Box<dyn Fn() + Send>,
    mouse_pos_fn: Box<dyn Fn() -> Option<(f64, f64)> + Send>,
    double_tap_timeout: Duration,
    modifier_state: ModifierState,
) {
    let mut detector = HotkeyDetector::with_timeout(double_tap_timeout);
    let mut pending_mouse_pos: Option<(f64, f64)> = None;
    let mut phase = Phase::Idle;
    info!("coordinator thread started");

    // After a trigger resolves: if the modifiers are still held, enter the
    // cycling phase; otherwise commit immediately so the UI runs its deferred
    // capture / no-op mode commit. Returns the next phase.
    let resolve_trigger = |is_double_tap: bool, held: bool| -> Phase {
        if held {
            Phase::Cycling { is_double_tap }
        } else {
            debug!(is_double_tap, "coordinator: cycle commit");
            let _ = tap_tx.send(TapEvent {
                action: TapAction::CycleCommit { is_double_tap },
                mouse_pos: None,
            });
            ctx.request_repaint();
            Phase::Idle
        }
    };

    loop {
        // Poll while waiting for a second tap or while cycling (so a modifier
        // release is noticed within ~50ms); block with zero CPU when idle.
        let polling = detector.is_pending() || matches!(phase, Phase::Cycling { .. });
        let event = if polling {
            match hotkey_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(e) => Some(e),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match hotkey_rx.recv() {
                Ok(e) => Some(e),
                Err(_) => break,
            }
        };

        let held = modifier_state.combo_held();

        if let Some(event) = event
            && event.state == HotKeyState::Pressed
        {
            match phase {
                Phase::Idle | Phase::AwaitingTrigger => match detector.on_press() {
                    TapAction::Pending => {
                        // Capture mouse position at first key press.
                        pending_mouse_pos = mouse_pos_fn();
                        phase = Phase::AwaitingTrigger;
                    }
                    TapAction::DoubleTap => {
                        debug!(held, "coordinator: double-tap trigger");
                        pre_show();
                        let _ = tap_tx.send(TapEvent {
                            action: TapAction::DoubleTap,
                            mouse_pos: pending_mouse_pos.take(),
                        });
                        ctx.request_repaint();
                        phase = resolve_trigger(true, held);
                    }
                    other => unreachable!("on_press returned {other:?}"),
                },
                Phase::Cycling { .. } => {
                    // Each further C tap while held advances the mode preview.
                    debug!("coordinator: cycle advance");
                    let _ = tap_tx.send(TapEvent {
                        action: TapAction::CycleAdvance,
                        mouse_pos: None,
                    });
                    ctx.request_repaint();
                }
            }
        }

        if matches!(phase, Phase::AwaitingTrigger) && detector.check_timeout() {
            debug!(held, "coordinator: single-tap trigger");
            pre_show();
            let _ = tap_tx.send(TapEvent {
                action: TapAction::SingleTap,
                mouse_pos: pending_mouse_pos.take(),
            });
            ctx.request_repaint();
            phase = resolve_trigger(false, held);
        }

        // Commit the cycle once the modifiers are released.
        if let Phase::Cycling { is_double_tap } = phase
            && !held
        {
            let _ = tap_tx.send(TapEvent {
                action: TapAction::CycleCommit { is_double_tap },
                mouse_pos: None,
            });
            ctx.request_repaint();
            phase = Phase::Idle;
        }
    }

    info!("coordinator thread exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn press_event() -> GlobalHotKeyEvent {
        GlobalHotKeyEvent {
            id: 1,
            state: HotKeyState::Pressed,
        }
    }

    fn release_event() -> GlobalHotKeyEvent {
        GlobalHotKeyEvent {
            id: 1,
            state: HotKeyState::Released,
        }
    }

    fn noop_mouse() -> Box<dyn Fn() -> Option<(f64, f64)> + Send> {
        Box::new(|| Some((100.0, 200.0)))
    }

    /// A modifier state seeded to a fixed held/released value.
    fn modifiers(held: bool) -> ModifierState {
        let s = ModifierState::default();
        s.set_combo_held(held);
        s
    }

    #[test]
    fn single_tap_sends_action_and_calls_pre_show() {
        let (htx, hrx) = mpsc::channel();
        let (ttx, trx) = mpsc::channel();
        let ctx = egui::Context::default();
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();

        let h = std::thread::spawn(move || {
            run(
                hrx,
                ttx,
                ctx,
                Box::new(move || {
                    c.fetch_add(1, Ordering::SeqCst);
                }),
                noop_mouse(),
                Duration::from_millis(500),
                modifiers(false),
            );
        });

        htx.send(press_event()).unwrap();
        // Wait for single-tap timeout (500ms + margin)
        let tap_event = trx.recv_timeout(Duration::from_millis(700)).unwrap();
        assert_eq!(tap_event.action, TapAction::SingleTap);
        assert!(tap_event.mouse_pos.is_some());
        assert_eq!(count.load(Ordering::SeqCst), 1);

        drop(htx);
        h.join().unwrap();
    }

    #[test]
    fn double_tap_sends_action() {
        let (htx, hrx) = mpsc::channel();
        let (ttx, trx) = mpsc::channel();
        let ctx = egui::Context::default();

        let h = std::thread::spawn(move || {
            run(hrx, ttx, ctx, Box::new(|| {}), noop_mouse(), Duration::from_millis(500), modifiers(false));
        });

        htx.send(press_event()).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        htx.send(press_event()).unwrap();

        let tap_event = trx.recv_timeout(Duration::from_millis(200)).unwrap();
        assert_eq!(tap_event.action, TapAction::DoubleTap);
        assert!(tap_event.mouse_pos.is_some());

        drop(htx);
        h.join().unwrap();
    }

    #[test]
    fn release_events_ignored() {
        let (htx, hrx) = mpsc::channel();
        let (ttx, trx) = mpsc::channel();
        let ctx = egui::Context::default();

        let h = std::thread::spawn(move || {
            run(hrx, ttx, ctx, Box::new(|| {}), noop_mouse(), Duration::from_millis(500), modifiers(false));
        });

        htx.send(release_event()).unwrap();
        assert!(trx.recv_timeout(Duration::from_millis(100)).is_err());

        drop(htx);
        h.join().unwrap();
    }

    #[test]
    fn exits_on_channel_disconnect() {
        let (htx, hrx) = mpsc::channel();
        let (ttx, _trx) = mpsc::channel();
        let ctx = egui::Context::default();

        let h = std::thread::spawn(move || {
            run(hrx, ttx, ctx, Box::new(|| {}), noop_mouse(), Duration::from_millis(500), modifiers(false));
        });

        drop(htx);
        h.join().unwrap();
    }

    #[test]
    fn cycling_advances_then_commits_on_release() {
        let (htx, hrx) = mpsc::channel();
        let (ttx, trx) = mpsc::channel();
        let ctx = egui::Context::default();
        let state = modifiers(true); // Ctrl+Shift held throughout the gesture
        let state_run = state.clone();

        let h = std::thread::spawn(move || {
            run(hrx, ttx, ctx, Box::new(|| {}), noop_mouse(), Duration::from_millis(500), state_run);
        });

        // Double-tap trigger while the modifiers are held.
        htx.send(press_event()).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        htx.send(press_event()).unwrap();
        assert_eq!(
            trx.recv_timeout(Duration::from_millis(300)).unwrap().action,
            TapAction::DoubleTap
        );

        // A further tap advances the cycle preview (no new trigger).
        htx.send(press_event()).unwrap();
        assert_eq!(
            trx.recv_timeout(Duration::from_millis(300)).unwrap().action,
            TapAction::CycleAdvance
        );

        // Releasing the modifiers commits the cycle.
        state.set_combo_held(false);
        assert_eq!(
            trx.recv_timeout(Duration::from_millis(300)).unwrap().action,
            TapAction::CycleCommit { is_double_tap: true }
        );

        drop(htx);
        h.join().unwrap();
    }

    #[test]
    fn trigger_commits_immediately_when_not_held() {
        // Modifiers already released at trigger time: no cycling, a single
        // CycleCommit follows the trigger so downstream handling is uniform.
        let (htx, hrx) = mpsc::channel();
        let (ttx, trx) = mpsc::channel();
        let ctx = egui::Context::default();

        let h = std::thread::spawn(move || {
            run(hrx, ttx, ctx, Box::new(|| {}), noop_mouse(), Duration::from_millis(500), modifiers(false));
        });

        htx.send(press_event()).unwrap();
        assert_eq!(
            trx.recv_timeout(Duration::from_millis(700)).unwrap().action,
            TapAction::SingleTap
        );
        assert_eq!(
            trx.recv_timeout(Duration::from_millis(200)).unwrap().action,
            TapAction::CycleCommit { is_double_tap: false }
        );

        drop(htx);
        h.join().unwrap();
    }
}
