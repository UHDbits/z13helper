//! Daemon UI-event subscription with reconnect backoff.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::app::AppState;
use crate::power;
use z13helper_core::debounce::PowerDebouncer;
use z13helper_core::{DaemonEvent, DaemonEventKind};

/// Events held while GTK is busy; the oldest is dropped beyond this.
const MAX_PENDING_EVENTS: usize = 32;
/// Repeated toggle presses closer together than this act once.
const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(50);

pub fn start(state: &Rc<AppState>, power: Rc<RefCell<PowerDebouncer>>) {
    let client = state.client.clone();
    let (tx, rx) = async_channel::bounded::<DaemonEvent>(MAX_PENDING_EVENTS);
    std::thread::spawn(move || {
        let mut backoff = Duration::from_millis(250);
        let mut last_error = None;
        loop {
            match client.subscribe(&[
                DaemonEventKind::GuiToggle,
                DaemonEventKind::ControllerAction,
                DaemonEventKind::PowerSourceChanged,
            ]) {
                Ok((events, _cancel)) => {
                    backoff = Duration::from_millis(250);
                    last_error = None;
                    while let Ok(event) = events.recv() {
                        let _ = tx.force_send(event);
                    }
                }
                Err(error) => {
                    let error = error.to_string();
                    if last_error.as_ref() != Some(&error) {
                        tracing::warn!(%error, "daemon event subscription failed; retrying");
                        last_error = Some(error);
                    }
                }
            }
            // Also after a stream ends, so an ack-then-drop daemon cannot spin.
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(15));
        }
    });
    let state = state.clone();
    glib::MainContext::default().spawn_local(async move {
        let mut last_toggle = Instant::now() - Duration::from_secs(1);
        while let Ok(event) = rx.recv().await {
            match event.kind {
                DaemonEventKind::GuiToggle if last_toggle.elapsed() >= TOGGLE_DEBOUNCE => {
                    last_toggle = Instant::now();
                    state.toggle_window();
                }
                DaemonEventKind::ControllerAction => {
                    if let Some(action) = event.action {
                        state.handle_controller_action(action);
                    }
                }
                DaemonEventKind::PowerSourceChanged => {
                    if let Some(on_battery) = event.on_battery {
                        power::on_resume(&state, &power, on_battery);
                    }
                }
                _ => {}
            }
        }
    });
}
