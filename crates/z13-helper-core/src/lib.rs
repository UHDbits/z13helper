//! Pure logic for z13-helper: profiles, apply sequencing, curve math, config.

pub mod apply;
pub mod config;
pub mod curve;
pub mod debounce;
pub mod label;
pub mod profile;

pub use apply::{apply_profile, ClientDaemon, Daemon};
pub use config::Config;
pub use profile::{Base, Profile};
