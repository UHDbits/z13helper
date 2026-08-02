//! Pure logic for z13helper: profiles, apply sequencing, curve math, config.

pub mod apply;
pub mod config;
pub mod curve;
pub mod debounce;
pub mod error;
pub mod label;
pub mod profile;
pub mod protocol;

pub use apply::{apply_request, Daemon};
pub use config::Config;
pub use error::DaemonError;
pub use profile::{Base, FanControlMode, Profile};
pub use protocol::*;
