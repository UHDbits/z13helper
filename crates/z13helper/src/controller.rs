//! Background controller-capture lease maintenance.
//!
//! The worker performs every daemon call. The GTK thread only sends desired
//! visibility changes through a channel, preserving the no-blocking-I/O rule.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use z13helper_client::Client;

const LEASE_REFRESH: Duration = Duration::from_secs(1);
const RELEASE_DELAY: Duration = Duration::from_millis(200);

pub struct ControllerCapture {
    mailbox: Arc<CaptureMailbox>,
}

#[derive(Default)]
struct CaptureState {
    desired: Option<bool>,
    closed: bool,
}

#[derive(Default)]
struct CaptureMailbox {
    state: Mutex<CaptureState>,
    changed: Condvar,
}

impl CaptureMailbox {
    fn replace(&self, desired: bool) {
        let mut state = self.state.lock().unwrap();
        state.desired = Some(desired);
        self.changed.notify_one();
    }

    fn wait(&self, timeout: Option<Duration>) -> (Option<bool>, bool) {
        let mut state = self.state.lock().unwrap();
        if let Some(timeout) = timeout {
            state = self
                .changed
                .wait_timeout_while(state, timeout, |state| {
                    state.desired.is_none() && !state.closed
                })
                .unwrap()
                .0;
        } else {
            state = self
                .changed
                .wait_while(state, |state| state.desired.is_none() && !state.closed)
                .unwrap();
        }
        (state.desired.take(), state.closed)
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        self.changed.notify_one();
    }
}

impl ControllerCapture {
    pub fn start(client: Client) -> Self {
        let mailbox = Arc::new(CaptureMailbox::default());
        let worker_mailbox = Arc::clone(&mailbox);
        std::thread::spawn(move || {
            let mut enabled = false;
            loop {
                let (update, closed) = worker_mailbox.wait(enabled.then_some(LEASE_REFRESH));
                if closed {
                    let _ = client.set_controller_capture(false);
                    break;
                }
                // An enabled lease is renewed after every timeout. A disabled
                // worker waits indefinitely for a replacement.
                let mut desired = update.unwrap_or(true);
                // Consume the dismiss button's release before handing the
                // controller back to the game. A re-show during this small
                // window supersedes the pending release.
                if enabled && !desired {
                    let (newer, closed) = worker_mailbox.wait(Some(RELEASE_DELAY));
                    if closed {
                        let _ = client.set_controller_capture(false);
                        break;
                    }
                    if let Some(newer) = newer {
                        desired = newer;
                    }
                }
                enabled = desired;
                if let Err(error) = client.set_controller_capture(enabled) {
                    tracing::warn!(%error, enabled, "could not update controller capture lease");
                }
            }
        });
        Self { mailbox }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.mailbox.replace(enabled);
    }
}

impl Drop for ControllerCapture {
    fn drop(&mut self) {
        self.mailbox.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_capture_state_coalesces_to_latest_value() {
        let mailbox = CaptureMailbox::default();
        mailbox.replace(true);
        mailbox.replace(false);
        assert_eq!(mailbox.wait(Some(Duration::ZERO)), (Some(false), false));
        assert_eq!(mailbox.wait(Some(Duration::ZERO)), (None, false));
    }

    #[test]
    fn close_wakes_a_waiting_lease_owner() {
        let mailbox = Arc::new(CaptureMailbox::default());
        let waiter = Arc::clone(&mailbox);
        let thread = std::thread::spawn(move || waiter.wait(None));
        mailbox.close();
        assert_eq!(thread.join().unwrap(), (None, true));
    }
}
