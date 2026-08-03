//! Daemon UI-event subscription with reconnect backoff.

use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::app::AppState;
use z13helper_core::{ControllerAction, DaemonEventKind};

enum UiEvent {
    Toggle,
    Controller(ControllerAction),
}

pub fn start(state: &Rc<AppState>) {
    let client = state.client.clone();
    let (tx, rx) = mpsc::channel::<UiEvent>();
    std::thread::spawn(move || {
        let mut backoff = Duration::from_millis(250);
        loop {
            match client.subscribe(&["gui-toggle", "controller-action"]) {
                Ok((events, _cancel)) => {
                    backoff = Duration::from_millis(250);
                    while let Ok(event) = events.recv() {
                        match (event.kind, event.action) {
                            (DaemonEventKind::GuiToggle, _) => {
                                let _ = tx.send(UiEvent::Toggle);
                            }
                            (DaemonEventKind::ControllerAction, Some(action)) => {
                                let _ = tx.send(UiEvent::Controller(action));
                            }
                            _ => {}
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
        while let Ok(event) = rx.try_recv() {
            match event {
                UiEvent::Toggle => toggled = true,
                UiEvent::Controller(action) => state.handle_controller_action(action),
            }
        }
        if toggled && last.elapsed() >= Duration::from_millis(50) {
            last = Instant::now();
            state.toggle_window();
        }
        glib::ControlFlow::Continue
    });
}
