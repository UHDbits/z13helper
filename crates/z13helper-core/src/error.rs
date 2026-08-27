use std::fmt;

use serde::{Deserialize, Serialize};

use crate::protocol::ClientId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    NotRunning,
    PermissionDenied,
    Timeout,
    OutcomeUnknown {
        client_id: ClientId,
        request_id: u64,
    },
    Rejected(String),
    Protocol(String),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning => write!(
                f,
                "z13helperd is not running — sudo systemctl start z13helperd.service"
            ),
            Self::PermissionDenied => write!(
                f,
                "permission denied on z13helperd socket — join the z13helper group and re-login"
            ),
            Self::Timeout => write!(f, "z13helperd timed out"),
            Self::OutcomeUnknown {
                client_id,
                request_id,
            } => write!(
                f,
                "z13helperd request outcome is unknown; query `z13helperctl outcome {client_id} {request_id}` before retrying"
            ),
            Self::Rejected(message) => write!(f, "{message}"),
            Self::Protocol(message) => write!(f, "protocol error: {message}"),
        }
    }
}

impl std::error::Error for DaemonError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    NotRunning,
    PermissionDenied,
    Timeout,
    Rejected,
    Protocol,
    Unsupported,
    Degraded,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WireError {
    pub code: ErrorCode,
    pub message: String,
}
