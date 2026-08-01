//! Unix socket client for the z13ctl daemon.
#![allow(dead_code)]

pub mod error;
pub mod types;
pub mod client;

pub use client::Client;
pub use error::DaemonError;
pub use types::*;
