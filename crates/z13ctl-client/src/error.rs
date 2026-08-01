use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    NotRunning,
    PermissionDenied,
    Timeout,
    Rejected(String),
    Protocol(String),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRunning => write!(
                f,
                "z13ctl daemon not running — systemctl --user start z13ctl.service"
            ),
            Self::PermissionDenied => write!(
                f,
                "permission denied on z13ctl socket — run `sudo z13ctl setup` and re-login"
            ),
            Self::Timeout => write!(f, "z13ctl daemon timed out"),
            Self::Rejected(msg) => write!(f, "{msg}"),
            Self::Protocol(msg) => write!(f, "protocol error: {msg}"),
        }
    }
}

impl std::error::Error for DaemonError {}
