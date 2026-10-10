//! Background controller-capture lease maintenance.
//!
//! The worker performs every daemon call. The GTK thread only sends desired
//! visibility changes through a channel, preserving the no-blocking-I/O rule.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use z13helper_client::Client;
use z13helper_core::Command;

const LEASE_REFRESH: Duration = Duration::from_secs(1);
const RELEASE_DELAY: Duration = Duration::from_millis(200);

/// Dropping this closes the channel, which releases capture.
pub struct ControllerCapture {
    desired: mpsc::Sender<bool>,
}

/// Coalesce queued visibility changes to the newest one. `None` once the
/// owner is gone: close wins over a still-queued request.
fn latest(updates: &Receiver<bool>, mut desired: bool) -> Option<bool> {
    loop {
        match updates.try_recv() {
            Ok(newer) => desired = newer,
            Err(TryRecvError::Empty) => return Some(desired),
            Err(TryRecvError::Disconnected) => return None,
        }
    }
}

impl ControllerCapture {
    pub fn start(client: Client) -> Self {
        let (desired, updates) = mpsc::channel();
        std::thread::spawn(move || {
            let mut enabled = false;
            loop {
                // An enabled lease is renewed after every timeout. A disabled
                // worker waits indefinitely for a replacement.
                let update = if enabled {
                    updates.recv_timeout(LEASE_REFRESH)
                } else {
                    updates.recv().map_err(RecvTimeoutError::from)
                };
                let mut desired = match update.map(|first| latest(&updates, first)) {
                    Ok(Some(desired)) => desired,
                    Err(RecvTimeoutError::Timeout) => true,
                    Ok(None) | Err(RecvTimeoutError::Disconnected) => break,
                };
                // Consume the dismiss button's release before handing the
                // controller back to the game. A re-show during this small
                // window supersedes the pending release.
                if enabled && !desired {
                    match updates
                        .recv_timeout(RELEASE_DELAY)
                        .map(|first| latest(&updates, first))
                    {
                        Ok(Some(newer)) => desired = newer,
                        Err(RecvTimeoutError::Timeout) => {}
                        Ok(None) | Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
                enabled = desired;
                if let Err(error) = client.send(Command::SetControllerCapture { enabled }) {
                    tracing::warn!(%error, enabled, "could not update controller capture lease");
                }
            }
            let _ = client.send(Command::SetControllerCapture { enabled: false });
        });
        Self { desired }
    }

    pub fn set_enabled(&self, enabled: bool) {
        let _ = self.desired.send(enabled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_capture_state_coalesces_to_latest_value() {
        let (tx, rx) = mpsc::channel();
        tx.send(true).unwrap();
        tx.send(false).unwrap();
        let first = rx.recv().unwrap();
        assert_eq!(latest(&rx, first), Some(false));
        tx.send(true).unwrap();
        drop(tx);
        assert_eq!(latest(&rx, false), None, "close wins over a queued request");
    }
}
