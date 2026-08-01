//! Placeholder — implemented in the client milestone.
use crate::error::DaemonError;

pub struct Client;

impl Client {
    pub fn new() -> Self {
        Self
    }

    pub fn socket_path() -> String {
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        format!("{runtime}/z13ctl/z13ctl.sock")
    }

    pub fn ping(&self) -> Result<(), DaemonError> {
        Err(DaemonError::NotRunning)
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}
