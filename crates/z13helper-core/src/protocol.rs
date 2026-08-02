use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::curve::Curve;
use crate::error::WireError;
use crate::profile::{Base, FanControlMode, Profile};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FanFloorConfig {
    pub engage_temp_c: i32,
    pub release_temp_c: i32,
    pub duty: u8,
    pub dwell_ms: u64,
}

impl Default for FanFloorConfig {
    fn default() -> Self {
        Self {
            engage_temp_c: 70,
            release_temp_c: 65,
            duty: 204,
            dwell_ms: 5_000,
        }
    }
}

impl FanFloorConfig {
    pub fn validate(self) -> Result<Self, String> {
        if !(60..=70).contains(&self.engage_temp_c) {
            return Err("floor engage temperature must be between 60 and 70°C".into());
        }
        if !(50..=65).contains(&self.release_temp_c) {
            return Err("floor release temperature must be between 50 and 65°C".into());
        }
        if self.release_temp_c > self.engage_temp_c - 5 {
            return Err("floor release temperature must be at least 5°C below engage".into());
        }
        if self.duty < 204 {
            return Err("high-power floor cannot be lower than 204 PWM".into());
        }
        if !(5_000..=30_000).contains(&self.dwell_ms) {
            return Err("floor dwell must be between 5 and 30 seconds".into());
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FloorEnforcement {
    #[default]
    Inactive,
    FirmwareArmed,
    DirectReleased,
    DirectEngaged,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct FloorState {
    pub enforcement: FloorEnforcement,
    pub armed: bool,
    pub engaged: bool,
    pub effective_min_duty: u8,
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
    #[serde(default)]
    pub base: Base,
    #[serde(default)]
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
    pub boot_sound: Option<i32>,
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
    pub undervolt_available: bool,
    #[serde(default)]
    pub temperature: Option<i32>,
    #[serde(default)]
    pub fan_rpms: [u32; 2],
    #[serde(default)]
    pub floor_config: FanFloorConfig,
    #[serde(default)]
    pub floor: FloorState,
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
            base: Base::Balanced,
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
            boot_sound: None,
            panel_overdrive: None,
            fan_curves: None,
            fan_control_mode: FanControlMode::Firmware,
            tdp: None,
            undervolt: None,
            undervolt_available: false,
            temperature: None,
            fan_rpms: [0; 2],
            floor_config: FanFloorConfig::default(),
            floor: FloorState::default(),
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
    pub base: Base,
    pub ppd_profile: Option<String>,
    pub power_limits: Option<TdpState>,
    pub fan_mode: FanControlMode,
    pub fan_curves: Option<[Curve; 2]>,
    pub undervolt: Option<i32>,
    pub floor: FanFloorConfig,
}

impl ApplyRequest {
    pub fn from_profile(profile: &Profile, floor: FanFloorConfig) -> Self {
        let power_limits = profile.apply_power_limits.then_some(TdpState {
            pl1_spl: profile.pl1_spl as i32,
            pl2_sppt: profile.pl2_sppt as i32,
            fppt: profile.fppt as i32,
            apu_sppt: profile.pl2_sppt as i32,
            platform_sppt: profile.pl2_sppt as i32,
        });
        Self {
            base: profile.base,
            ppd_profile: profile.ppd_profile.clone(),
            power_limits,
            fan_mode: profile.fan_control_mode,
            fan_curves: profile.apply_fan_curve.then_some(profile.fan_curves),
            undervolt: profile.apply_undervolt.then_some(profile.cpu_co),
            floor,
        }
    }

    pub fn effective_pl1(&self) -> u32 {
        self.power_limits
            .map(|tdp| tdp.pl1_spl.max(0) as u32)
            .unwrap_or_else(|| self.base.stock_ppt().0)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.floor.validate()?;
        if let Some(curves) = &self.fan_curves {
            for curve in curves {
                crate::curve::validate(curve).map_err(|error| error.to_string())?;
            }
        }
        if let Some(tdp) = self.power_limits {
            for (name, value) in [
                ("PL1", tdp.pl1_spl),
                ("PL2", tdp.pl2_sppt),
                ("FPPT", tdp.fppt),
                ("APU SPPT", tdp.apu_sppt),
                ("platform SPPT", tdp.platform_sppt),
            ] {
                if !(5..=93).contains(&value) {
                    return Err(format!("{name} must be between 5 and 93 watts"));
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
    Apply {
        request: ApplyRequest,
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
    SetBootSound {
        enabled: bool,
    },
    SetLighting {
        device: String,
        state: LightingState,
    },
    ReleaseFans,
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
}

impl DaemonEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StateChanged => "state-changed",
            Self::GuiToggle => "gui-toggle",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DaemonEvent {
    pub kind: DaemonEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
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
            event: None,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_bounds_reject_weaker_policy() {
        let config = FanFloorConfig {
            duty: 203,
            ..FanFloorConfig::default()
        };
        assert!(config.validate().is_err());
        let config = FanFloorConfig {
            release_temp_c: 68,
            ..FanFloorConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn request_roundtrip_is_versioned() {
        let request = WireRequest {
            version: PROTOCOL_VERSION,
            command: Command::GetState,
        };
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.contains("\"version\":1"));
        assert!(text.contains("\"cmd\":\"get-state\""));
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
    fn complete_apply_is_prevalidated() {
        let mut profile = Profile::builtin("turbo", "Turbo", Base::Performance);
        profile.apply_power_limits = true;
        profile.pl1_spl = 80;
        profile.pl2_sppt = 90;
        profile.fppt = 94;
        let request = ApplyRequest::from_profile(&profile, FanFloorConfig::default());
        assert!(request.validate().is_err());
    }
}
