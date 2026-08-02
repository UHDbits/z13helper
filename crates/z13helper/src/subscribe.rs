//! Daemon gui-toggle subscription with reconnect backoff.

use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::app::AppState;
use z13helper_core::DaemonEventKind;

pub fn start(state: &Rc<AppState>) {
    let client = state.client.clone();
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut backoff = Duration::from_millis(250);
        loop {
            match client.subscribe(&["gui-toggle"]) {
                Ok((events, _cancel)) => {
                    backoff = Duration::from_millis(250);
                    while let Ok(event) = events.recv() {
                        if event.kind == DaemonEventKind::GuiToggle {
                            let _ = tx.send(());
                        }
                    }
                }
                Err(_) => std::thread::sleep(backoff),
            }
            backoff = (backoff * 2).min(Duration::from_secs(15));
        }
    });
    let state = state.clone();
    let mut last = Instant::now() - Duration::from_secs(1);
    glib::timeout_add_local(Duration::from_millis(50), move || {
        let mut toggled = false;
        while rx.try_recv().is_ok() {
            toggled = true;
        }
        if toggled && last.elapsed() >= Duration::from_millis(50) {
            last = Instant::now();
            state.toggle_window();
        }
        glib::ControlFlow::Continue
    });
}
