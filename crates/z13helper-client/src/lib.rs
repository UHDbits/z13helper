//! Unix-socket client for the privileged `z13helperd` system daemon.

mod client;

pub use client::{Client, SubscribeCancel};
pub use z13helper_core::protocol::{DaemonState as State, LightingState, TdpState, UndervoltState};
pub use z13helper_core::DaemonError;
