use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::protocol::ClientId;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DaemonError {
    #[error("z13helperd is not running — sudo systemctl start z13helperd.service")]
    NotRunning,
    #[error("permission denied on z13helperd socket — join the z13helper group and re-login")]
    PermissionDenied,
    #[error("z13helperd timed out")]
    Timeout,
    #[error(
        "z13helperd request outcome is unknown; query `z13helperctl outcome {client_id} {request_id}` before retrying"
    )]
    OutcomeUnknown {
        client_id: ClientId,
        request_id: u64,
    },
    #[error("{0}")]
    Rejected(String),
    #[error("protocol error: {0}")]
    Protocol(String),
}

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
