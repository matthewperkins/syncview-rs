//! File dialogs that don't freeze the window. On Linux, rfd's dialog (an xdg-desktop-portal
//! request) blocks the calling thread until the user chooses; on the UI thread that stops the
//! window answering the compositor's pings, and Hyprland (or GNOME, KDE) then offers to kill it as
//! "not responding". So on Linux the dialog runs on its own thread and the UI polls for the
//! answer. macOS and Windows dialogs run their own event loop on the UI thread, so there it is
//! called inline, as before.

use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

/// A file dialog that is open (or has just been answered); `what` says what the answer is for.
pub struct Pending<T> {
    rx: Receiver<Option<PathBuf>>,
    pub what: T,
}

/// Show the dialog that `show` builds and runs (e.g. `|| rfd::FileDialog::new().pick_file()`).
pub fn open<T>(ctx: &egui::Context, what: T, show: impl FnOnce() -> Option<PathBuf> + Send + 'static) -> Pending<T> {
    let (tx, rx) = channel();
    if cfg!(target_os = "linux") {
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(show());
            ctx.request_repaint();
        });
    } else {
        let _ = tx.send(show());
    }
    Pending { rx, what }
}

impl<T> Pending<T> {
    /// The answer once the dialog has closed: Some(path), or Some(None) if it was cancelled.
    pub fn answer(&self) -> Option<Option<PathBuf>> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(None),
        }
    }
}

/// `slot`'s answer, if its dialog has closed (the slot is then emptied).
pub fn take_answer<T>(slot: &mut Option<Pending<T>>) -> Option<(T, Option<PathBuf>)> {
    let answer = slot.as_ref()?.answer()?;
    slot.take().map(|p| (p.what, answer))
}

#[test]
fn a_slow_dialog_does_not_block() {
    let ctx = egui::Context::default();
    let t0 = std::time::Instant::now();
    let mut slot = Some(open(&ctx, 7, || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        Some(PathBuf::from("/x"))
    }));
    if cfg!(target_os = "linux") {
        assert!(t0.elapsed().as_millis() < 100, "open() waited for the dialog");
        assert!(take_answer(&mut slot).is_none() && slot.is_some());
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    assert_eq!(take_answer(&mut slot), Some((7, Some(PathBuf::from("/x")))));
    assert!(slot.is_none());
}
