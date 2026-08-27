//! Daemon UI-event subscription with reconnect backoff.

use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::app::AppState;
use crate::services::power;
use z13helper_core::{ControllerAction, DaemonEventKind};

const MAX_PENDING_CONTROLLER_ACTIONS: usize = 32;

#[derive(Default)]
struct UiMailbox {
    toggle: bool,
    power_source: Option<bool>,
    controller_actions: VecDeque<ControllerAction>,
}

impl UiMailbox {
    fn push_controller(&mut self, action: ControllerAction) {
        if self.controller_actions.len() == MAX_PENDING_CONTROLLER_ACTIONS {
            self.controller_actions.pop_front();
        }
        self.controller_actions.push_back(action);
    }
}

pub fn start(state: &Rc<AppState>) {
    let client = state.client.clone();
    let mailbox = Arc::new(Mutex::new(UiMailbox::default()));
    let worker_mailbox = Arc::clone(&mailbox);
    std::thread::spawn(move || {
        let mut backoff = Duration::from_millis(250);
        loop {
            match client.subscribe(&[
                DaemonEventKind::GuiToggle,
                DaemonEventKind::ControllerAction,
                DaemonEventKind::PowerSourceChanged,
            ]) {
                Ok((events, _cancel)) => {
                    backoff = Duration::from_millis(250);
                    while let Ok(event) = events.recv() {
                        let mut mailbox = worker_mailbox.lock().unwrap();
                        match event.kind {
                            DaemonEventKind::GuiToggle => {
                                mailbox.toggle = true;
                            }
                            DaemonEventKind::ControllerAction => {
                                if let Some(action) = event.action {
                                    mailbox.push_controller(action);
                                }
                            }
                            DaemonEventKind::PowerSourceChanged => {
                                if let Some(on_battery) = event.on_battery {
                                    mailbox.power_source = Some(on_battery);
                                }
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
        let (toggle, power_source, controller_actions) = {
            let mut mailbox = mailbox.lock().unwrap();
            (
                std::mem::take(&mut mailbox.toggle),
                mailbox.power_source.take(),
                std::mem::take(&mut mailbox.controller_actions),
            )
        };
        for action in controller_actions {
            state.handle_controller_action(action);
        }
        if let Some(on_battery) = power_source {
            power::on_resume(&state, on_battery);
        }
        if toggle && last.elapsed() >= Duration::from_millis(50) {
            last = Instant::now();
            state.toggle_window();
        }
        glib::ControlFlow::Continue
    });
}
