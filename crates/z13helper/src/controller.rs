//! Background controller-capture lease maintenance.
//!
//! The worker performs every daemon call. The GTK thread only sends desired
//! visibility changes through a channel, preserving the no-blocking-I/O rule.

use std::sync::mpsc;
use std::time::Duration;

use z13helper_client::Client;

const LEASE_REFRESH: Duration = Duration::from_secs(1);
const RELEASE_DELAY: Duration = Duration::from_millis(200);

pub struct ControllerCapture {
    sender: mpsc::Sender<bool>,
}

impl ControllerCapture {
    pub fn start(client: Client) -> Self {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut enabled = false;
            loop {
                let mut desired = if enabled {
                    match receiver.recv_timeout(LEASE_REFRESH) {
                        Ok(desired) => desired,
                        Err(mpsc::RecvTimeoutError::Timeout) => true,
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            let _ = client.set_controller_capture(false);
                            break;
                        }
                    }
                } else {
                    match receiver.recv() {
                        Ok(desired) => desired,
                        Err(_) => break,
                    }
                };
                // Consume the dismiss button's release before handing the
                // controller back to the game. A re-show during this small
                // window supersedes the pending release.
                if enabled && !desired {
                    match receiver.recv_timeout(RELEASE_DELAY) {
                        Ok(newer) => desired = newer,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            let _ = client.set_controller_capture(false);
                            break;
                        }
                    }
                }
                enabled = desired;
                if let Err(error) = client.set_controller_capture(enabled) {
                    tracing::warn!(%error, enabled, "could not update controller capture lease");
                }
            }
        });
        Self { sender }
    }

    pub fn set_enabled(&self, enabled: bool) {
        let _ = self.sender.send(enabled);
    }
}
