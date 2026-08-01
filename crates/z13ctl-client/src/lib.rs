//! Unix socket client for the z13ctl daemon.

mod client;
mod error;
mod manual_fan;
mod types;

pub use client::{Client, SubscribeCancel};
pub use error::DaemonError;
pub use manual_fan::{ManualFanClient, ManualFanPoint, ManualFanStatus};
pub use types::*;
