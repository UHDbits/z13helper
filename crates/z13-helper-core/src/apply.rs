//! Placeholder — implemented in the apply milestone.
use crate::profile::Profile;
use z13ctl_client::DaemonError;

pub trait Daemon {
    fn profile_set(&self, base: crate::profile::Base) -> Result<(), DaemonError>;
    fn tdp_set(&self, pl1: u32, pl2: u32, pl3: u32, force: bool) -> Result<(), DaemonError>;
    fn fan_curve_set(&self, curve: &[[i32; 2]; 8]) -> Result<(), DaemonError>;
    fn undervolt_set(&self, cpu_co: i32) -> Result<(), DaemonError>;
}

pub fn apply_profile(_daemon: &impl Daemon, _profile: &Profile, _uv_ok: bool) -> Result<(), DaemonError> {
    Ok(())
}
