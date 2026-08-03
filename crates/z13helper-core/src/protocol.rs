use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::curve::Curve;
use crate::error::WireError;
use crate::profile::{stock_ppt, FanControlMode, Profile};

pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FanHysteresis {
    pub up: u8,
    pub down: u8,
}

pub const MAX_FAN_TEMPERATURE_AVERAGE_SECONDS: u8 = 15;

impl Default for FanHysteresis {
    fn default() -> Self {
        Self { up: 3, down: 3 }
    }
}

impl FanHysteresis {
    pub fn validate(self) -> Result<Self, String> {
        if !(1..=5).contains(&self.up) || !(1..=5).contains(&self.down) {
            return Err("fan hysteresis values must be between 1 and 5".into());
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct LightingState {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub color2: String,
    #[serde(default)]
    pub speed: String,
    #[serde(default)]
    pub brightness: i32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TdpState {
    pub pl1_spl: i32,
    pub pl2_sppt: i32,
    pub fppt: i32,
    pub apu_sppt: i32,
    pub platform_sppt: i32,
}

/// Measured complete stock table for each PPD-selected firmware policy.
/// ASUS PPT sysfs nodes retain the last values written and expose no reset or
/// factory-read operation, so selecting an unmodified profile rewrites this
/// table explicitly.
pub fn stock_tdp(ppd_profile: Option<&str>) -> Option<TdpState> {
    let (pl1_spl, pl2_sppt, fppt) = stock_ppt(ppd_profile);
    ppd_profile.map(|_| TdpState {
        pl1_spl: pl1_spl as i32,
        pl2_sppt: pl2_sppt as i32,
        fppt: fppt as i32,
        apu_sppt: 70,
        platform_sppt: 70,
    })
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UndervoltState {
    pub cpu_co: i32,
    pub active: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OverrideState {
    pub power: bool,
    pub fans: bool,
    pub undervolt: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Capabilities {
    pub ppd_available: bool,
    pub ppd_profiles: Vec<String>,
    pub firmware_fans: bool,
    pub direct_fans: bool,
    pub undervolt: bool,
    pub keyboard_lighting: bool,
    pub lightbar_lighting: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Telemetry {
    pub temperature_c: Option<i32>,
    pub fan_rpms: [u32; 2],
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct BatteryTelemetry {
    pub charge_percent: Option<u8>,
    pub status: Option<String>,
    #[serde(default)]
    pub power_microwatts: Option<u64>,
    #[serde(default)]
    pub health_percent: Option<u8>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Health {
    pub degraded: bool,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DaemonState {
    #[serde(default)]
    pub generation: u64,
    pub profile: Option<String>,
    #[serde(default)]
    pub overrides: OverrideState,
    #[serde(default)]
    pub ppd_profile: Option<String>,
    #[serde(default)]
    pub lighting: LightingState,
    #[serde(default)]
    pub devices: Option<HashMap<String, LightingState>>,
    #[serde(default)]
    pub battery_limit: Option<i32>,
    #[serde(default)]
    pub battery_one_time_charge: bool,
    #[serde(default)]
    pub battery: BatteryTelemetry,
    #[serde(default)]
    pub panel_overdrive: Option<i32>,
    #[serde(default)]
    pub fan_curves: Option<[Curve; 2]>,
    #[serde(default)]
    pub fan_control_mode: FanControlMode,
    #[serde(default)]
    pub tdp: Option<TdpState>,
    #[serde(default)]
    pub undervolt: Option<UndervoltState>,
    #[serde(default)]
    pub cpu_temp_limit: Option<u8>,
    #[serde(default)]
    pub undervolt_available: bool,
    #[serde(default)]
    pub temperature: Option<i32>,
    #[serde(default)]
    pub fan_rpms: [u32; 2],
    #[serde(default)]
    pub fan_hysteresis: FanHysteresis,
    #[serde(default = "crate::profile::default_fan_temperature_average_seconds")]
    pub fan_temperature_average_seconds: u8,
    #[serde(default)]
    pub direct_fan_duties: [u8; 2],
    #[serde(default)]
    pub high_power_fan_protection: bool,
    #[serde(default)]
    pub disable_high_power_fan_protection: bool,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub telemetry: Telemetry,
    #[serde(default)]
    pub health: Health,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub degraded: bool,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self {
            generation: 0,
            profile: Some("balanced".into()),
            overrides: OverrideState::default(),
            ppd_profile: Some("balanced".into()),
            lighting: LightingState {
                enabled: true,
                mode: "static".into(),
                color: "FF0000".into(),
                color2: "000000".into(),
                speed: "normal".into(),
                brightness: 3,
            },
            devices: None,
            battery_limit: None,
            battery_one_time_charge: false,
            battery: BatteryTelemetry::default(),
            panel_overdrive: None,
            fan_curves: None,
            fan_control_mode: FanControlMode::Firmware,
            tdp: None,
            undervolt: None,
            cpu_temp_limit: None,
            undervolt_available: false,
            temperature: None,
            fan_rpms: [0; 2],
            fan_hysteresis: FanHysteresis::default(),
            fan_temperature_average_seconds:
                crate::profile::default_fan_temperature_average_seconds(),
            direct_fan_duties: [0; 2],
            high_power_fan_protection: false,
            disable_high_power_fan_protection: false,
            capabilities: Capabilities::default(),
            telemetry: Telemetry::default(),
            health: Health::default(),
            warnings: Vec::new(),
            degraded: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ApplyRequest {
    pub ppd_profile: Option<String>,
    pub power_limits: Option<TdpState>,
    pub fan_mode: FanControlMode,
    pub fan_curves: Option<[Curve; 2]>,
    pub undervolt: Option<i32>,
    #[serde(default = "crate::profile::default_cpu_temp_limit")]
    pub cpu_temp_limit: u8,
    #[serde(default)]
    pub fan_hysteresis: FanHysteresis,
    #[serde(default = "crate::profile::default_fan_temperature_average_seconds")]
    pub fan_temperature_average_seconds: u8,
    #[serde(default)]
    pub disable_high_power_fan_protection: bool,
}

impl ApplyRequest {
    pub fn from_profile(profile: &Profile, disable_high_power_fan_protection: bool) -> Self {
        let power_limits = profile.apply_power_limits.then_some(TdpState {
            pl1_spl: profile.pl1_spl as i32,
            pl2_sppt: profile.pl2_sppt as i32,
            fppt: profile.fppt as i32,
            apu_sppt: profile.pl2_sppt as i32,
            platform_sppt: profile.pl2_sppt as i32,
        });
        let effective_pl1 = power_limits
            .or_else(|| stock_tdp(profile.ppd_profile.as_deref()))
            .map(|tdp| tdp.pl1_spl.max(0) as u32)
            .unwrap_or(0);
        let needs_protected_curve = effective_pl1 >= crate::curve::HIGH_POWER_THRESHOLD_W
            && !disable_high_power_fan_protection;
        let fan_curves = if profile.unified_fan_control {
            [profile.fan_curves[0], profile.fan_curves[0]]
        } else {
            profile.fan_curves
        };
        Self {
            ppd_profile: profile.ppd_profile.clone(),
            power_limits,
            fan_mode: profile.fan_control_mode,
            fan_curves: (profile.apply_fan_curve || needs_protected_curve).then_some(fan_curves),
            undervolt: profile.apply_undervolt.then_some(profile.cpu_co),
            cpu_temp_limit: profile.cpu_temp_limit,
            fan_hysteresis: FanHysteresis {
                up: profile.fan_hysteresis_up,
                down: profile.fan_hysteresis_down,
            },
            fan_temperature_average_seconds: profile.fan_temperature_average_seconds,
            disable_high_power_fan_protection,
        }
    }

    pub fn effective_power_limits(&self) -> Option<TdpState> {
        self.power_limits
            .or_else(|| stock_tdp(self.ppd_profile.as_deref()))
    }

    pub fn effective_pl1(&self) -> u32 {
        self.effective_power_limits()
            .map(|tdp| tdp.pl1_spl.max(0) as u32)
            .unwrap_or_else(|| stock_ppt(self.ppd_profile.as_deref()).0)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.fan_hysteresis.validate()?;
        if self.fan_temperature_average_seconds > MAX_FAN_TEMPERATURE_AVERAGE_SECONDS {
            return Err(format!(
                "fan temperature averaging must be between 0 and {MAX_FAN_TEMPERATURE_AVERAGE_SECONDS} seconds"
            ));
        }
        if let Some(curves) = &self.fan_curves {
            for curve in curves {
                crate::curve::validate(curve).map_err(|error| error.to_string())?;
            }
        }
        if let Some(tdp) = self.power_limits {
            for (name, value, maximum) in [
                ("PL1", tdp.pl1_spl, 93),
                ("PL2", tdp.pl2_sppt, 93),
                ("FPPT", tdp.fppt, 120),
                ("APU SPPT", tdp.apu_sppt, 93),
                ("platform SPPT", tdp.platform_sppt, 93),
            ] {
                if !(5..=maximum).contains(&value) {
                    return Err(format!("{name} must be between 5 and {maximum} watts"));
                }
            }
            if tdp.pl2_sppt < tdp.pl1_spl || tdp.fppt < tdp.pl2_sppt {
                return Err("power limits must satisfy PL1 <= PL2 <= FPPT".into());
            }
        }
        if self
            .undervolt
            .is_some_and(|offset| !(-40..=0).contains(&offset))
        {
            return Err("Curve Optimizer offset must be between -40 and 0".into());
        }
        if !(80..=99).contains(&self.cpu_temp_limit) {
            return Err("APU temperature limit must be between 80 and 99°C".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProbeReply {
    pub model: String,
    pub ec_version: u8,
    pub fan_count: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Command {
    GetState,
    Probe,
    GetFactoryFanCurves {
        ppd_profiles: Vec<String>,
    },
    Apply {
        request: ApplyRequest,
    },
    ApplyUndervoltOnce {
        offset: i32,
    },
    SetBatteryLimit {
        limit: i32,
    },
    SetBatteryOneTimeCharge {
        enabled: bool,
    },
    SetPanelOverdrive {
        enabled: bool,
    },
    SetLighting {
        device: String,
        state: LightingState,
    },
    ReleaseFans,
    SetControllerCapture {
        enabled: bool,
    },
    Subscribe {
        events: Vec<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WireRequest {
    pub version: u32,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ApplyResponse {
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DaemonEventKind {
    StateChanged,
    GuiToggle,
    ControllerAction,
    PowerSourceChanged,
}

impl DaemonEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StateChanged => "state-changed",
            Self::GuiToggle => "gui-toggle",
            Self::ControllerAction => "controller-action",
            Self::PowerSourceChanged => "power-source-changed",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControllerAction {
    Up,
    Down,
    Left,
    Right,
    Accept,
    Back,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DaemonEvent {
    pub kind: DaemonEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<ControllerAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_battery: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WireResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<DaemonState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply: Option<ApplyResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeReply>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub factory_fan_curves: Option<HashMap<String, [Curve; 2]>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<DaemonEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

impl WireResponse {
    pub fn success() -> Self {
        Self {
            ok: true,
            state: None,
            apply: None,
            probe: None,
            factory_fan_curves: None,
            event: None,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hysteresis_bounds_match_exposed_levels() {
        assert!(FanHysteresis { up: 1, down: 5 }.validate().is_ok());
        assert!(FanHysteresis { up: 0, down: 3 }.validate().is_err());
        assert!(FanHysteresis { up: 3, down: 6 }.validate().is_err());
    }

    #[test]
    fn temperature_average_is_profile_configurable_and_bounded() {
        let mut profile = Profile::builtin("balanced", "Balanced");
        assert_eq!(
            ApplyRequest::from_profile(&profile, false).fan_temperature_average_seconds,
            6
        );

        profile.fan_temperature_average_seconds = 0;
        assert!(ApplyRequest::from_profile(&profile, false)
            .validate()
            .is_ok());

        profile.fan_temperature_average_seconds = MAX_FAN_TEMPERATURE_AVERAGE_SECONDS + 1;
        assert!(ApplyRequest::from_profile(&profile, false)
            .validate()
            .is_err());
    }

    #[test]
    fn request_roundtrip_is_versioned() {
        let request = WireRequest {
            version: PROTOCOL_VERSION,
            command: Command::GetState,
        };
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.contains("\"version\":2"));
        assert!(text.contains("\"cmd\":\"get-state\""));
    }

    #[test]
    fn controller_capture_and_action_roundtrip() {
        let request = WireRequest {
            version: PROTOCOL_VERSION,
            command: Command::SetControllerCapture { enabled: true },
        };
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.contains("\"cmd\":\"set-controller-capture\""));
        let event = DaemonEvent {
            kind: DaemonEventKind::ControllerAction,
            action: Some(ControllerAction::Accept),
            generation: None,
            on_battery: None,
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"kind":"controller-action","action":"accept"}"#
        );
    }

    #[test]
    fn power_source_resume_event_roundtrips_the_post_resume_source() {
        let event = DaemonEvent {
            kind: DaemonEventKind::PowerSourceChanged,
            action: None,
            generation: None,
            on_battery: Some(true),
        };
        let decoded: DaemonEvent = serde_json::from_str(&serde_json::to_string(&event).unwrap())
            .expect("power-source event should decode");
        assert_eq!(decoded.kind, DaemonEventKind::PowerSourceChanged);
        assert_eq!(decoded.on_battery, Some(true));
    }

    #[test]
    fn one_time_charge_command_roundtrips() {
        let request = WireRequest {
            version: PROTOCOL_VERSION,
            command: Command::SetBatteryOneTimeCharge { enabled: true },
        };
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.contains("\"cmd\":\"set-battery-one-time-charge\""));
        assert!(text.contains("\"enabled\":true"));
        let decoded: WireRequest = serde_json::from_str(&text).unwrap();
        assert!(matches!(
            decoded.command,
            Command::SetBatteryOneTimeCharge { enabled: true }
        ));
    }

    #[test]
    fn factory_fan_curve_command_roundtrips() {
        let request = WireRequest {
            version: PROTOCOL_VERSION,
            command: Command::GetFactoryFanCurves {
                ppd_profiles: vec!["power-saver".into(), "balanced".into()],
            },
        };
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.contains("\"cmd\":\"get-factory-fan-curves\""));
        let decoded: WireRequest = serde_json::from_str(&text).unwrap();
        assert!(matches!(
            decoded.command,
            Command::GetFactoryFanCurves { ppd_profiles } if ppd_profiles.len() == 2
        ));
    }

    #[test]
    fn one_shot_undervolt_command_roundtrips() {
        let request = WireRequest {
            version: PROTOCOL_VERSION,
            command: Command::ApplyUndervoltOnce { offset: -20 },
        };
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.contains("\"cmd\":\"apply-undervolt-once\""));
        let decoded: WireRequest = serde_json::from_str(&text).unwrap();
        assert!(matches!(
            decoded.command,
            Command::ApplyUndervoltOnce { offset: -20 }
        ));
    }

    #[test]
    fn complete_apply_is_prevalidated() {
        let mut profile = Profile::builtin("turbo", "Turbo");
        profile.apply_power_limits = true;
        profile.pl1_spl = 80;
        profile.pl2_sppt = 90;
        profile.fppt = 121;
        let request = ApplyRequest::from_profile(&profile, false);
        assert!(request.validate().is_err());

        profile.fppt = 120;
        let request = ApplyRequest::from_profile(&profile, false);
        assert!(request.validate().is_ok());

        let mut request = request;
        request.cpu_temp_limit = 79;
        assert!(request.validate().is_err());
    }

    #[test]
    fn high_power_request_carries_profiles_measured_curve_for_protection() {
        let mut profile = Profile::builtin("turbo", "Turbo");
        profile.apply_power_limits = true;
        profile.pl1_spl = 81;
        profile.pl2_sppt = 90;
        profile.fppt = 100;
        profile.fan_curves[0][0] = [42, 43];
        let request = ApplyRequest::from_profile(&profile, false);
        assert_eq!(request.fan_curves.unwrap()[0][0], [42, 43]);

        let overridden = ApplyRequest::from_profile(&profile, true);
        assert!(overridden.fan_curves.is_none());
    }

    #[test]
    fn unified_fan_control_applies_the_first_curve_to_both_fans() {
        let mut profile = Profile::builtin("turbo", "Turbo");
        profile.apply_fan_curve = true;
        profile.unified_fan_control = true;
        profile.fan_curves[0][0] = [42, 43];
        profile.fan_curves[1][0] = [1, 2];

        let curves = ApplyRequest::from_profile(&profile, false)
            .fan_curves
            .unwrap();
        assert_eq!(curves[0], curves[1]);
        assert_eq!(curves[0][0], [42, 43]);
    }
}
