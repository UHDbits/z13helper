//! Pure logic for z13helper: profiles, apply sequencing, curve math, config.

pub mod apply;
pub mod config;
pub mod curve;
pub mod debounce;
pub mod error;
pub mod label;
pub mod profile;
pub mod protocol;

pub use apply::{Daemon, apply_request};
pub use config::Config;
pub use curve::HIGH_POWER_THRESHOLD_W;
pub use error::DaemonError;
pub use profile::{FanControlMode, Profile, stock_fan_curves, stock_ppt};
pub use protocol::*;
