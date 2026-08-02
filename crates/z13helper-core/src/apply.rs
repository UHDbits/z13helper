//! Fail-closed hardware apply sequencing.

use crate::curve::{Curve, TDP_MAX_SAFE};
use crate::error::DaemonError;
use crate::profile::{Base, FanControlMode};
use crate::protocol::{ApplyRequest, TdpState};

/// Hardware operations owned by `z13helperd`.
pub trait Daemon {
    fn profile_set(&mut self, base: Base) -> Result<(), DaemonError>;
    fn ppd_set(&mut self, profile: Option<&str>) -> Result<Option<String>, DaemonError>;
    fn tdp_set(&mut self, limits: TdpState, force: bool) -> Result<(), DaemonError>;
    fn firmware_fans_set(
        &mut self,
        curves: &[Curve; 2],
        effective_pl1: u32,
    ) -> Result<(), DaemonError>;
    fn direct_fans_set(
        &mut self,
        curves: &[Curve; 2],
        effective_pl1: u32,
    ) -> Result<(), DaemonError>;
    fn fans_release(&mut self) -> Result<(), DaemonError>;
    fn undervolt_set(&mut self, cpu_co: i32) -> Result<(), DaemonError>;
    fn undervolt_available(&self) -> bool;
}

fn selected_curves(request: &ApplyRequest) -> [Curve; 2] {
    request.fan_curves.unwrap_or_else(|| {
        let stock = request.base.stock_fan_curve();
        [stock, stock]
    })
}

fn set_fans(
    daemon: &mut impl Daemon,
    request: &ApplyRequest,
    curves: &[Curve; 2],
    pl1: u32,
) -> Result<(), DaemonError> {
    match request.fan_mode {
        FanControlMode::Firmware => daemon.firmware_fans_set(curves, pl1),
        FanControlMode::Direct => daemon.direct_fans_set(curves, pl1),
    }
}

/// Apply one flattened request. The daemon serializes calls to this function.
///
/// A base write always runs and restores stock PPT. For high-power requests,
/// fan protection is installed before custom PPT. For safe requests, power is
/// lowered before fan protection is relaxed.
pub fn apply_request(
    daemon: &mut impl Daemon,
    request: &ApplyRequest,
) -> Result<Vec<String>, DaemonError> {
    request.validate().map_err(DaemonError::Rejected)?;

    daemon.profile_set(request.base)?;
    let mut warnings = Vec::new();
    if let Some(warning) = daemon.ppd_set(request.ppd_profile.as_deref())? {
        warnings.push(warning);
    }

    let pl1 = request.effective_pl1();
    let curves = selected_curves(request);
    if pl1 > TDP_MAX_SAFE {
        set_fans(daemon, request, &curves, pl1)?;
        if let Some(limits) = request.power_limits {
            daemon.tdp_set(limits, true)?;
        }
    } else {
        if let Some(limits) = request.power_limits {
            daemon.tdp_set(limits, false)?;
        }
        if request.fan_curves.is_some() {
            set_fans(daemon, request, &curves, pl1)?;
        } else {
            daemon.fans_release()?;
        }
    }

    if let Some(offset) = request.undervolt {
        if daemon.undervolt_available() {
            daemon.undervolt_set(offset)?;
        } else {
            warnings.push("ryzen_smu is unavailable; undervolt was not applied".into());
        }
    }
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::profile::Profile;
    use crate::protocol::FanFloorConfig;

    #[derive(Default)]
    struct RecordingDaemon {
        calls: RefCell<Vec<String>>,
        fail_at: Option<&'static str>,
        uv: bool,
    }

    impl RecordingDaemon {
        fn call(&self, name: impl Into<String>) -> Result<(), DaemonError> {
            let name = name.into();
            let key = name.split(':').next().unwrap_or_default().to_owned();
            self.calls.borrow_mut().push(name);
            if self.fail_at == Some(key.as_str()) {
                Err(DaemonError::Rejected(format!("{key} failed")))
            } else {
                Ok(())
            }
        }
    }

    impl Daemon for RecordingDaemon {
        fn profile_set(&mut self, base: Base) -> Result<(), DaemonError> {
            self.call(format!("profile:{}", base.as_str()))
        }
        fn ppd_set(&mut self, profile: Option<&str>) -> Result<Option<String>, DaemonError> {
            self.call(format!("ppd:{}", profile.unwrap_or("off")))?;
            Ok(None)
        }
        fn tdp_set(&mut self, limits: TdpState, force: bool) -> Result<(), DaemonError> {
            self.call(format!("tdp:{}:force={force}", limits.pl1_spl))
        }
        fn firmware_fans_set(&mut self, _: &[Curve; 2], _: u32) -> Result<(), DaemonError> {
            self.call("firmware-fans")
        }
        fn direct_fans_set(&mut self, _: &[Curve; 2], _: u32) -> Result<(), DaemonError> {
            self.call("direct-fans")
        }
        fn fans_release(&mut self) -> Result<(), DaemonError> {
            self.call("fans-release")
        }
        fn undervolt_set(&mut self, offset: i32) -> Result<(), DaemonError> {
            self.call(format!("undervolt:{offset}"))
        }
        fn undervolt_available(&self) -> bool {
            self.uv
        }
    }

    fn request(pl1: u32) -> ApplyRequest {
        let mut profile = Profile::builtin("gaming", "Gaming", Base::Balanced);
        profile.apply_power_limits = true;
        profile.pl1_spl = pl1;
        profile.pl2_sppt = pl1.max(80);
        profile.fppt = pl1.max(90);
        profile.apply_fan_curve = true;
        ApplyRequest::from_profile(&profile, FanFloorConfig::default())
    }

    #[test]
    fn high_power_prepares_fans_before_tdp() {
        let mut daemon = RecordingDaemon::default();
        apply_request(&mut daemon, &request(80)).unwrap();
        let calls = daemon.calls.borrow();
        let fan = calls.iter().position(|v| v == "firmware-fans").unwrap();
        let tdp = calls.iter().position(|v| v.starts_with("tdp:")).unwrap();
        assert!(fan < tdp);
    }

    #[test]
    fn fan_failure_abandons_high_power() {
        let mut daemon = RecordingDaemon {
            fail_at: Some("firmware-fans"),
            ..Default::default()
        };
        assert!(apply_request(&mut daemon, &request(80)).is_err());
        assert!(!daemon.calls.borrow().iter().any(|v| v.starts_with("tdp:")));
    }

    #[test]
    fn safe_power_is_lowered_before_fans() {
        let mut daemon = RecordingDaemon::default();
        apply_request(&mut daemon, &request(60)).unwrap();
        let calls = daemon.calls.borrow();
        let fan = calls.iter().position(|v| v == "firmware-fans").unwrap();
        let tdp = calls.iter().position(|v| v.starts_with("tdp:")).unwrap();
        assert!(tdp < fan);
    }

    #[test]
    fn base_always_runs_and_stock_releases_fans() {
        let profile = Profile::builtin("balanced", "Balanced", Base::Balanced);
        let request = ApplyRequest::from_profile(&profile, FanFloorConfig::default());
        let mut daemon = RecordingDaemon::default();
        apply_request(&mut daemon, &request).unwrap();
        assert_eq!(
            daemon.calls.into_inner(),
            ["profile:balanced", "ppd:balanced", "fans-release"]
        );
    }
}
