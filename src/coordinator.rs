use std::sync::mpsc;
use std::time::Duration;

use eframe::egui;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use tracing::info;

use crate::hotkey::{HotkeyDetector, TapAction, TapEvent};

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
/// - During double-tap window (500ms): polls with `recv_timeout(50ms)`.
pub fn run(
    hotkey_rx: mpsc::Receiver<GlobalHotKeyEvent>,
    tap_tx: mpsc::Sender<TapEvent>,
    ctx: egui::Context,
    pre_show: Box<dyn Fn() + Send>,
    mouse_pos_fn: Box<dyn Fn() -> Option<(f64, f64)> + Send>,
) {
    let mut detector = HotkeyDetector::new();
    let mut pending_mouse_pos: Option<(f64, f64)> = None;
    info!("coordinator thread started");

    loop {
        // Event-driven: block when idle, poll only during double-tap window.
        let event = if detector.is_pending() {
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

        if let Some(event) = event
            && event.state == HotKeyState::Pressed
        {
            match detector.on_press() {
                TapAction::Pending => {
                    // Capture mouse position at first key press.
                    pending_mouse_pos = mouse_pos_fn();
                }
                TapAction::DoubleTap => {
                    pre_show();
                    let _ = tap_tx.send(TapEvent {
                        action: TapAction::DoubleTap,
                        mouse_pos: pending_mouse_pos.take(),
                    });
                    ctx.request_repaint();
                }
                TapAction::SingleTap => unreachable!("on_press never returns SingleTap"),
            }
        }

        if detector.check_timeout() {
            pre_show();
            let _ = tap_tx.send(TapEvent {
                action: TapAction::SingleTap,
                mouse_pos: pending_mouse_pos.take(),
            });
            ctx.request_repaint();
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
            run(hrx, ttx, ctx, Box::new(|| {}), noop_mouse());
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
            run(hrx, ttx, ctx, Box::new(|| {}), noop_mouse());
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
            run(hrx, ttx, ctx, Box::new(|| {}), noop_mouse());
        });

        drop(htx);
        h.join().unwrap();
    }
}
