//! Unix socket client for the z13ctl daemon.

mod client;
mod error;
mod types;

pub use client::{Client, SubscribeCancel};
pub use error::DaemonError;
pub use types::*;
