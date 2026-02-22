use std::sync::mpsc;
use std::time::Duration;

use eframe::egui;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use tracing::info;

use crate::hotkey::{HotkeyDetector, TapAction};

/// Run the coordinator loop on the current thread (blocking).
///
/// Detects single/double-tap hotkey patterns and forwards [`TapAction`] to the
/// UI thread via `tap_tx`. This is 100% common code — platform-specific window
/// show logic is injected via the `pre_show` callback.
///
/// The loop is event-driven:
/// - Idle: blocks on `recv()` (zero CPU).
/// - During double-tap window (500ms): polls with `recv_timeout(50ms)`.
pub fn run(
    hotkey_rx: mpsc::Receiver<GlobalHotKeyEvent>,
    tap_tx: mpsc::Sender<TapAction>,
    ctx: egui::Context,
    pre_show: Box<dyn Fn() + Send>,
) {
    let mut detector = HotkeyDetector::new();
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

        if let Some(event) = event {
            if event.state == HotKeyState::Pressed {
                match detector.on_press() {
                    TapAction::Pending => {}
                    TapAction::DoubleTap => {
                        pre_show();
                        let _ = tap_tx.send(TapAction::DoubleTap);
                        ctx.request_repaint();
                    }
                    TapAction::SingleTap => unreachable!("on_press never returns SingleTap"),
                }
            }
        }

        if detector.check_timeout() {
            pre_show();
            let _ = tap_tx.send(TapAction::SingleTap);
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
            );
        });

        htx.send(press_event()).unwrap();
        // Wait for single-tap timeout (500ms + margin)
        let action = trx.recv_timeout(Duration::from_millis(700)).unwrap();
        assert_eq!(action, TapAction::SingleTap);
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
            run(hrx, ttx, ctx, Box::new(|| {}));
        });

        htx.send(press_event()).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        htx.send(press_event()).unwrap();

        let action = trx.recv_timeout(Duration::from_millis(200)).unwrap();
        assert_eq!(action, TapAction::DoubleTap);

        drop(htx);
        h.join().unwrap();
    }

    #[test]
    fn release_events_ignored() {
        let (htx, hrx) = mpsc::channel();
        let (ttx, trx) = mpsc::channel();
        let ctx = egui::Context::default();

        let h = std::thread::spawn(move || {
            run(hrx, ttx, ctx, Box::new(|| {}));
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
            run(hrx, ttx, ctx, Box::new(|| {}));
        });

        drop(htx);
        h.join().unwrap();
    }
}
