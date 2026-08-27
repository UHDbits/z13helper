//! Fail-closed hardware apply sequencing.

use crate::curve::{Curve, HIGH_POWER_THRESHOLD_W};
use crate::error::DaemonError;
use crate::profile::{FanControlMode, stock_fan_curves};
use crate::protocol::{ApplyRequest, TdpState};

/// Hardware operations owned by `z13helperd`.
pub trait Daemon {
    fn ppd_set(&mut self, profile: Option<&str>) -> Result<Option<String>, DaemonError>;
    fn tdp_set(&mut self, limits: TdpState) -> Result<(), DaemonError>;
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
    fn cpu_temp_limit_set(&mut self, temperature_c: u8) -> Result<(), DaemonError>;
    fn undervolt_set(&mut self, cpu_co: i32) -> Result<(), DaemonError>;
    fn undervolt_available(&self) -> bool;
}

fn selected_curves(request: &ApplyRequest) -> [Curve; 2] {
    request
        .fan_curves
        .unwrap_or_else(|| stock_fan_curves(request.ppd_profile.as_deref()))
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
/// PPD selects the firmware policy first. For high-power requests, fan
/// protection is installed before custom PPT. For safe requests, power is
/// lowered before fan protection is relaxed.
pub fn apply_request(
    daemon: &mut impl Daemon,
    request: &ApplyRequest,
) -> Result<Vec<String>, DaemonError> {
    request.validate().map_err(DaemonError::Rejected)?;

    let mut warnings = Vec::new();
    if let Some(warning) = daemon.ppd_set(request.ppd_profile.as_deref())? {
        warnings.push(warning);
    }

    let pl1 = request.effective_pl1();
    let limits = request.effective_power_limits();
    let curves = selected_curves(request);
    if pl1 >= HIGH_POWER_THRESHOLD_W && !request.disable_high_power_fan_protection {
        set_fans(daemon, request, &curves, pl1)?;
        if let Some(limits) = limits {
            daemon.tdp_set(limits)?;
        }
    } else {
        if let Some(limits) = limits {
            daemon.tdp_set(limits)?;
        }
        if request.fan_curves.is_some() {
            set_fans(daemon, request, &curves, pl1)?;
        } else {
            daemon.fans_release()?;
        }
    }

    if daemon.undervolt_available() {
        daemon.cpu_temp_limit_set(request.cpu_temp_limit)?;
        if let Some(offset) = request.undervolt {
            daemon.undervolt_set(offset)?;
        }
    } else {
        warnings.push("ryzen_smu is unavailable; APU temperature limit was not applied".into());
        if request.undervolt.is_some() {
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
        fn ppd_set(&mut self, profile: Option<&str>) -> Result<Option<String>, DaemonError> {
            self.call(format!("ppd:{}", profile.unwrap_or("off")))?;
            Ok(None)
        }
        fn tdp_set(&mut self, limits: TdpState) -> Result<(), DaemonError> {
            self.call(format!(
                "tdp:{}/{}/{}/{}/{}",
                limits.pl1_spl, limits.pl2_sppt, limits.fppt, limits.apu_sppt, limits.platform_sppt
            ))
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
        fn cpu_temp_limit_set(&mut self, temperature_c: u8) -> Result<(), DaemonError> {
            self.call(format!("cpu-temp:{temperature_c}"))
        }
        fn undervolt_set(&mut self, offset: i32) -> Result<(), DaemonError> {
            self.call(format!("undervolt:{offset}"))
        }
        fn undervolt_available(&self) -> bool {
            self.uv
        }
    }

    fn request(pl1: u32) -> ApplyRequest {
        let mut profile = Profile::builtin("gaming", "Gaming");
        profile.apply_power_limits = true;
        profile.pl1_spl = pl1;
        profile.pl2_sppt = pl1.max(80);
        profile.fppt = pl1.max(90);
        profile.apply_fan_curve = true;
        ApplyRequest::from_profile(&profile, false)
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
    fn every_apply_stage_stops_before_a_later_hardware_stage() {
        let mut request = request(80);
        request.undervolt = Some(-10);
        for stage in ["ppd", "firmware-fans", "tdp", "cpu-temp", "undervolt"] {
            let mut daemon = RecordingDaemon {
                fail_at: Some(stage),
                uv: true,
                ..Default::default()
            };
            assert!(
                apply_request(&mut daemon, &request).is_err(),
                "stage {stage}"
            );
            let calls = daemon.calls.into_inner();
            let failed = calls
                .iter()
                .position(|call| call.split(':').next() == Some(stage))
                .expect("the injected stage was called");
            assert_eq!(calls.len(), failed + 1, "stage {stage} ran later work");
        }
    }

    #[test]
    fn confirmed_override_does_not_install_protected_fans() {
        let mut request = request(80);
        request.disable_high_power_fan_protection = true;
        request.fan_curves = None;
        let mut daemon = RecordingDaemon::default();
        apply_request(&mut daemon, &request).unwrap();
        let calls = daemon.calls.into_inner();
        assert!(calls.iter().any(|call| call == "fans-release"));
        assert!(!calls.iter().any(|call| call == "firmware-fans"));
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
    fn temperature_limit_is_applied_before_undervolt() {
        let mut request = request(60);
        request.cpu_temp_limit = 88;
        request.undervolt = Some(-10);
        let mut daemon = RecordingDaemon {
            uv: true,
            ..Default::default()
        };
        apply_request(&mut daemon, &request).unwrap();
        let calls = daemon.calls.into_inner();
        let temperature = calls.iter().position(|call| call == "cpu-temp:88").unwrap();
        let undervolt = calls
            .iter()
            .position(|call| call == "undervolt:-10")
            .unwrap();
        assert!(temperature < undervolt);
    }

    #[test]
    fn ppd_runs_and_stock_releases_fans() {
        let profile = Profile::builtin("balanced", "Balanced");
        let request = ApplyRequest::from_profile(&profile, false);
        let mut daemon = RecordingDaemon::default();
        apply_request(&mut daemon, &request).unwrap();
        assert_eq!(
            daemon.calls.into_inner(),
            ["ppd:balanced", "tdp:52/71/70/70/70", "fans-release"]
        );
    }

    #[test]
    fn stock_profiles_write_distinct_power_tables() {
        for (id, expected) in [
            ("silent", "tdp:40/55/55/70/70"),
            ("balanced", "tdp:52/71/70/70/70"),
            ("turbo", "tdp:70/86/86/70/70"),
        ] {
            let profile = Profile::builtin(id, id);
            let request = ApplyRequest::from_profile(&profile, false);
            let mut daemon = RecordingDaemon::default();
            apply_request(&mut daemon, &request).unwrap();
            assert!(
                daemon
                    .calls
                    .into_inner()
                    .iter()
                    .any(|call| call == expected)
            );
        }
    }
}
