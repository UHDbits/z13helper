//! Profile apply algorithm.
//!
//! Ordering is not negotiable:
//! 1. `profile-set` base — ALWAYS, even if unchanged (clears prior overrides)
//! 2. TDP (if apply_power_limits) — before fans
//! 3. Fan curve (if apply_fan_curve) — rejected locally if floor violated
//! 4. Undervolt (if apply_undervolt && available)

use crate::curve::{self, CurveError};
use crate::profile::{Base, FanControlMode, Profile};
use z13ctl_client::DaemonError;

/// Hardware operations needed by the apply algorithm.
pub trait Daemon {
    fn profile_set(&self, base: Base) -> Result<(), DaemonError>;
    fn manual_fan_release(&self) -> Result<(), DaemonError>;
    fn tdp_set(&self, pl1: u32, pl2: u32, pl3: u32, force: bool) -> Result<(), DaemonError>;
    fn fan_curve_set(&self, curve: &[[i32; 2]; 8]) -> Result<(), DaemonError>;
    fn manual_fan_set(&self, curve: &[[i32; 2]; 8]) -> Result<(), DaemonError>;
    fn undervolt_set(&self, cpu_co: i32) -> Result<(), DaemonError>;
}

/// Apply a GUI profile to the daemon's single `custom` slot.
///
/// `undervolt_available` comes from `get-state` — never probe the SMU yourself.
pub fn apply_profile(
    daemon: &impl Daemon,
    profile: &Profile,
    undervolt_available: bool,
) -> Result<(), DaemonError> {
    // 1. Unconditional stock base — clears prior overrides and sets PPD.
    daemon.profile_set(profile.base)?;
    // Direct EC control is outside z13ctl. Release any outgoing direct mode
    // after the stock base succeeds, before power/fan overrides are applied.
    daemon.manual_fan_release()?;

    // Effective PL1 for fan-floor checks: custom if applying, else stock.
    let effective_pl1 = if profile.apply_power_limits {
        profile.pl1_spl
    } else {
        profile.base.stock_ppt().0
    };

    // 2. TDP before fans.
    if profile.apply_power_limits {
        let force = profile.pl1_spl > curve::TDP_MAX_SAFE;
        daemon.tdp_set(profile.pl1_spl, profile.pl2_sppt, profile.fppt, force)?;
    }

    // 3. Fan curve.
    if profile.apply_fan_curve {
        if let Err(CurveError::BelowFloor { pwm, temp, min }) =
            curve::validate_against_floor(&profile.fan_curve, effective_pl1)
        {
            return Err(DaemonError::Rejected(format!(
                "PWM {pwm} at {temp}°C is below minimum {min} (80%) required when sustained TDP is above {}W",
                curve::TDP_MAX_SAFE
            )));
        }
        match profile.fan_control_mode {
            FanControlMode::Firmware => daemon.fan_curve_set(&profile.fan_curve)?,
            FanControlMode::Direct => daemon.manual_fan_set(&profile.fan_curve)?,
        }
    }

    // 4. Undervolt (conditional on availability).
    if profile.apply_undervolt && undervolt_available {
        daemon.undervolt_set(profile.cpu_co)?;
    }

    Ok(())
}

fn companion_apply_error(error: DaemonError) -> DaemonError {
    match error {
        DaemonError::NotRunning | DaemonError::PermissionDenied | DaemonError::Timeout => {
            DaemonError::Rejected(z13ctl_client::ManualFanClient::describe_error(&error))
        }
        other => other,
    }
}

/// Adapter that implements [`Daemon`] for [`z13ctl_client::Client`].
pub struct ClientDaemon<'a> {
    pub daemon: &'a z13ctl_client::Client,
    pub manual_fan: &'a z13ctl_client::ManualFanClient,
}

impl<'a> ClientDaemon<'a> {
    pub fn new(
        daemon: &'a z13ctl_client::Client,
        manual_fan: &'a z13ctl_client::ManualFanClient,
    ) -> Self {
        Self { daemon, manual_fan }
    }
}

impl Daemon for ClientDaemon<'_> {
    fn profile_set(&self, base: Base) -> Result<(), DaemonError> {
        self.daemon.profile_set(base.as_str())
    }

    fn manual_fan_release(&self) -> Result<(), DaemonError> {
        match self.manual_fan.release() {
            // Firmware mode must remain usable when the optional companion is
            // not installed. A running companion still must acknowledge release.
            Err(DaemonError::NotRunning) => Ok(()),
            Err(error) => Err(companion_apply_error(error)),
            Ok(()) => Ok(()),
        }
    }

    fn tdp_set(&self, pl1: u32, pl2: u32, pl3: u32, force: bool) -> Result<(), DaemonError> {
        // `set` is mandatory even when pl1/pl2/pl3 are supplied.
        self.daemon
            .tdp_set(pl1, Some(pl1), Some(pl2), Some(pl3), force)
    }

    fn fan_curve_set(&self, curve: &[[i32; 2]; 8]) -> Result<(), DaemonError> {
        self.daemon.fan_curve_set(curve)
    }

    fn manual_fan_set(&self, curve: &[[i32; 2]; 8]) -> Result<(), DaemonError> {
        self.manual_fan
            .enable(curve)
            .map(|_| ())
            .map_err(companion_apply_error)
    }

    fn undervolt_set(&self, cpu_co: i32) -> Result<(), DaemonError> {
        self.daemon.undervolt_set(cpu_co)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct RecordingDaemon {
        calls: RefCell<Vec<String>>,
        fail_at: RefCell<Option<String>>,
    }

    impl Daemon for RecordingDaemon {
        fn profile_set(&self, base: Base) -> Result<(), DaemonError> {
            self.calls
                .borrow_mut()
                .push(format!("profile_set:{}", base.as_str()));
            self.check_fail("profile_set")
        }

        fn manual_fan_release(&self) -> Result<(), DaemonError> {
            self.calls.borrow_mut().push("manual_fan_release".into());
            self.check_fail("manual_fan_release")
        }

        fn tdp_set(&self, pl1: u32, pl2: u32, pl3: u32, force: bool) -> Result<(), DaemonError> {
            self.calls
                .borrow_mut()
                .push(format!("tdp_set:{pl1}:{pl2}:{pl3}:force={force}"));
            self.check_fail("tdp_set")
        }

        fn fan_curve_set(&self, _curve: &[[i32; 2]; 8]) -> Result<(), DaemonError> {
            self.calls.borrow_mut().push("fan_curve_set".into());
            self.check_fail("fan_curve_set")
        }

        fn manual_fan_set(&self, _curve: &[[i32; 2]; 8]) -> Result<(), DaemonError> {
            self.calls.borrow_mut().push("manual_fan_set".into());
            self.check_fail("manual_fan_set")
        }

        fn undervolt_set(&self, cpu_co: i32) -> Result<(), DaemonError> {
            self.calls
                .borrow_mut()
                .push(format!("undervolt_set:{cpu_co}"));
            self.check_fail("undervolt_set")
        }
    }

    impl RecordingDaemon {
        fn check_fail(&self, step: &str) -> Result<(), DaemonError> {
            if self.fail_at.borrow().as_deref() == Some(step) {
                Err(DaemonError::Rejected(format!("{step} failed")))
            } else {
                Ok(())
            }
        }
    }

    fn full_custom() -> Profile {
        let mut p = Profile::builtin("gaming", "Gaming", Base::Balanced);
        p.builtin = false;
        p.apply_power_limits = true;
        p.pl1_spl = 60;
        p.pl2_sppt = 70;
        p.fppt = 70;
        p.apply_fan_curve = true;
        p.apply_undervolt = true;
        p.cpu_co = -20;
        p
    }

    #[test]
    fn full_sequence_order() {
        let d = RecordingDaemon::default();
        apply_profile(&d, &full_custom(), true).unwrap();
        assert_eq!(
            *d.calls.borrow(),
            vec![
                "profile_set:balanced".to_string(),
                "manual_fan_release".to_string(),
                "tdp_set:60:70:70:force=false".to_string(),
                "fan_curve_set".to_string(),
                "undervolt_set:-20".to_string(),
            ]
        );
    }

    #[test]
    fn profile_set_always_runs() {
        let d = RecordingDaemon::default();
        let p = Profile::builtin("balanced", "Balanced", Base::Balanced);
        apply_profile(&d, &p, false).unwrap();
        assert_eq!(
            *d.calls.borrow(),
            vec![
                "profile_set:balanced".to_string(),
                "manual_fan_release".to_string()
            ]
        );
    }

    #[test]
    fn force_when_pl1_above_75() {
        let d = RecordingDaemon::default();
        let mut p = full_custom();
        p.pl1_spl = 80;
        p.pl2_sppt = 85;
        p.fppt = 90;
        // Raise entire curve above the floor so local validation passes.
        for pt in &mut p.fan_curve {
            pt[1] = 210;
        }
        apply_profile(&d, &p, false).unwrap();
        assert!(d.calls.borrow()[2].contains("force=true"));
    }

    #[test]
    fn skips_undervolt_when_unavailable() {
        let d = RecordingDaemon::default();
        apply_profile(&d, &full_custom(), false).unwrap();
        assert!(!d.calls.borrow().iter().any(|c| c.starts_with("undervolt")));
    }

    #[test]
    fn rejects_curve_below_floor_locally() {
        let d = RecordingDaemon::default();
        let mut p = full_custom();
        p.pl1_spl = 80;
        // Default curve has points well below 204.
        let err = apply_profile(&d, &p, false).unwrap_err();
        assert!(matches!(err, DaemonError::Rejected(_)));
        // Must not have reached fan_curve_set.
        assert!(!d.calls.borrow().iter().any(|c| c == "fan_curve_set"));
    }

    #[test]
    fn tdp_before_fans() {
        let d = RecordingDaemon::default();
        let mut p = full_custom();
        p.apply_undervolt = false;
        apply_profile(&d, &p, false).unwrap();
        let calls = d.calls.borrow();
        let tdp = calls.iter().position(|c| c.starts_with("tdp_set")).unwrap();
        let fan = calls.iter().position(|c| c == "fan_curve_set").unwrap();
        assert!(tdp < fan);
    }

    #[test]
    fn direct_mode_uses_companion_in_fan_step() {
        let d = RecordingDaemon::default();
        let mut p = full_custom();
        p.fan_control_mode = FanControlMode::Direct;
        apply_profile(&d, &p, false).unwrap();
        let calls = d.calls.borrow();
        assert!(calls.iter().any(|call| call == "manual_fan_set"));
        assert!(!calls.iter().any(|call| call == "fan_curve_set"));
        let tdp = calls
            .iter()
            .position(|call| call.starts_with("tdp_set"))
            .unwrap();
        let fan = calls
            .iter()
            .position(|call| call == "manual_fan_set")
            .unwrap();
        assert!(tdp < fan);
    }

    #[test]
    fn release_failure_stops_before_tdp() {
        let d = RecordingDaemon {
            fail_at: RefCell::new(Some("manual_fan_release".into())),
            ..Default::default()
        };
        assert!(apply_profile(&d, &full_custom(), true).is_err());
        assert_eq!(
            *d.calls.borrow(),
            vec![
                "profile_set:balanced".to_string(),
                "manual_fan_release".to_string()
            ]
        );
    }

    #[test]
    fn stops_on_profile_set_failure() {
        let d = RecordingDaemon {
            fail_at: RefCell::new(Some("profile_set".into())),
            ..Default::default()
        };
        assert!(apply_profile(&d, &full_custom(), true).is_err());
        assert_eq!(d.calls.borrow().len(), 1);
    }
}
