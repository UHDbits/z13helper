use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::curve::Curve;
use crate::error::WireError;
use crate::profile::{FanControlMode, Profile, stock_ppt};

pub const PROTOCOL_VERSION: u32 = 3;

/// Maximum encoded size, including the terminating newline, of one v3 NDJSON
/// frame in either direction.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// A process-scoped, unpredictable client identity used to namespace request
/// IDs in the daemon's bounded outcome cache. It is serialized as fixed-width
/// hexadecimal so no JSON-number precision is involved for external tools.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClientId(u128);

impl ClientId {
    pub const fn new(value: u128) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    pub const fn get(self) -> u128 {
        self.0
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

impl FromStr for ClientId {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("client ID must be exactly 32 hexadecimal digits");
        }
        let value = u128::from_str_radix(text, 16).map_err(|_| "client ID is not hexadecimal")?;
        Self::new(value).ok_or("client ID must be non-zero")
    }
}

impl Serialize for ClientId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ClientId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A non-zero identifier for one client request. Request IDs are deliberately
/// opaque to the daemon; clients may use the same ID when reconnecting to
/// retrieve an already accepted request outcome.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct RequestId(u64);

impl<'de> Deserialize<'de> for RequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("request ID must be non-zero"))
    }
}

impl RequestId {
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for RequestId {
    type Error = &'static str;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value).ok_or("request ID must be non-zero")
    }
}

/// Progress and terminal state of an accepted request. A queued request can
/// expire or lose its peer before hardware starts; started work always reaches
/// `Completed`, even if its response socket has gone away.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestOutcome {
    Queued,
    Started,
    Completed,
    Expired,
    Disconnected,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    let ppd_profile =
        ppd_profile.filter(|profile| crate::profile::is_known_ppd_profile(profile))?;
    let (pl1_spl, pl2_sppt, fppt) = stock_ppt(Some(ppd_profile));
    Some(TdpState {
        pl1_spl: pl1_spl as i32,
        pl2_sppt: pl2_sppt as i32,
        fppt: fppt as i32,
        apu_sppt: 70,
        platform_sppt: 70,
    })
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UndervoltState {
    pub cpu_co: i32,
    pub active: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OverrideState {
    pub power: bool,
    pub fans: bool,
    pub undervolt: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct Telemetry {
    pub temperature_c: Option<i32>,
    pub fan_rpms: [u32; 2],
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BatteryTelemetry {
    pub charge_percent: Option<u8>,
    pub status: Option<String>,
    #[serde(default)]
    pub power_microwatts: Option<u64>,
    #[serde(default)]
    pub health_percent: Option<u8>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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

/// The protocol-v3 status representation. This is deliberately separate from
/// [`DaemonState`]: the latter is the schema-v1 persisted snapshot and retains
/// its historical fields for on-disk compatibility, while this DTO is the
/// single status contract exposed to clients.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireStatus {
    pub generation: u64,
    pub profile: Option<String>,
    pub overrides: OverrideState,
    pub ppd_profile: Option<String>,
    pub devices: BTreeMap<String, LightingState>,
    pub battery_limit: Option<i32>,
    pub battery_one_time_charge: bool,
    pub battery: BatteryTelemetry,
    pub panel_overdrive: Option<bool>,
    pub fan_curves: Option<[Curve; 2]>,
    pub fan_control_mode: FanControlMode,
    pub tdp: Option<TdpState>,
    pub undervolt: Option<UndervoltState>,
    pub cpu_temp_limit: Option<u8>,
    pub fan_hysteresis: FanHysteresis,
    pub fan_temperature_average_seconds: u8,
    pub direct_fan_duties: [u8; 2],
    pub disable_high_power_fan_protection: bool,
    pub capabilities: WireCapabilities,
    pub telemetry: Telemetry,
    pub health: Health,
}

/// Capabilities that have an in-tree protocol consumer. Constant hardware
/// facts and capabilities unused by the UI/CLI are intentionally not exposed.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireCapabilities {
    pub ppd_profiles: Vec<String>,
    pub direct_fans: bool,
    pub undervolt: bool,
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
#[serde(deny_unknown_fields)]
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
        if let Some(profile) = self.ppd_profile.as_deref()
            && !crate::profile::is_known_ppd_profile(profile)
        {
            return Err(format!("unknown PPD profile {profile:?}"));
        }
        if self.fan_mode == FanControlMode::Direct && self.fan_curves.is_none() {
            return Err("direct fan mode requires complete CPU and GPU curves".into());
        }
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
#[serde(deny_unknown_fields)]
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
        events: Vec<EventTopic>,
    },
    GetOutcome {
        target_client_id: ClientId,
        target_request_id: RequestId,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct WireRequest {
    pub version: u32,
    pub client_id: ClientId,
    pub request_id: RequestId,
    #[serde(flatten)]
    pub command: Command,
}

impl<'de> Deserialize<'de> for WireRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| serde::de::Error::custom("request envelope must be an object"))?;
        let command_name = object
            .get("cmd")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("request command must be a string"))?;
        let mut allowed = vec!["version", "client_id", "request_id", "cmd"];
        allowed.extend(match command_name {
            "get-factory-fan-curves" => ["ppd_profiles"].as_slice(),
            "apply" => ["request"].as_slice(),
            "apply-undervolt-once" => ["offset"].as_slice(),
            "set-battery-limit" => ["limit"].as_slice(),
            "set-battery-one-time-charge" | "set-panel-overdrive" | "set-controller-capture" => {
                ["enabled"].as_slice()
            }
            "set-lighting" => ["device", "state"].as_slice(),
            "subscribe" => ["events"].as_slice(),
            "get-outcome" => ["target_client_id", "target_request_id"].as_slice(),
            "get-state" | "probe" | "release-fans" => [].as_slice(),
            _ => [].as_slice(),
        });
        if let Some(unknown) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(serde::de::Error::custom(format!(
                "unknown request field {unknown:?}"
            )));
        }
        let version = object
            .get("version")
            .cloned()
            .ok_or_else(|| serde::de::Error::custom("missing request version"))
            .and_then(|value| serde_json::from_value(value).map_err(serde::de::Error::custom))?;
        let client_id = object
            .get("client_id")
            .cloned()
            .ok_or_else(|| serde::de::Error::custom("missing client ID"))
            .and_then(|value| serde_json::from_value(value).map_err(serde::de::Error::custom))?;
        let request_id = object
            .get("request_id")
            .cloned()
            .ok_or_else(|| serde::de::Error::custom("missing request ID"))
            .and_then(|value| serde_json::from_value(value).map_err(serde::de::Error::custom))?;
        let mut command_object = object.clone();
        command_object.remove("version");
        command_object.remove("client_id");
        command_object.remove("request_id");
        let command = serde_json::from_value(serde_json::Value::Object(command_object))
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            version,
            client_id,
            request_id,
            command,
        })
    }
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
    pub const ALL: [Self; 4] = [
        Self::StateChanged,
        Self::GuiToggle,
        Self::ControllerAction,
        Self::PowerSourceChanged,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::StateChanged => "state-changed",
            Self::GuiToggle => "gui-toggle",
            Self::ControllerAction => "controller-action",
            Self::PowerSourceChanged => "power-source-changed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

/// The event topics accepted by the subscription command. This alias keeps
/// the wire-facing event name while making daemon subscriptions typed.
pub type EventTopic = DaemonEventKind;

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
    pub on_battery: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireResponse {
    pub version: u32,
    /// The response correlation ID. It is absent only for a malformed frame
    /// that did not contain a valid request envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub outcome: RequestOutcome,
    /// Set only by a GetOutcome response; regular responses describe their own
    /// request identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_client_id: Option<ClientId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome_request_id: Option<RequestId>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<WireStatus>,
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
    pub fn success(request_id: RequestId) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: Some(request_id),
            outcome: RequestOutcome::Completed,
            outcome_client_id: None,
            outcome_request_id: None,
            ok: true,
            state: None,
            apply: None,
            probe: None,
            factory_fan_curves: None,
            event: None,
            error: None,
        }
    }

    pub fn progress(request_id: RequestId, outcome: RequestOutcome) -> Self {
        let mut response = Self::success(request_id);
        response.outcome = outcome;
        response
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
        assert!(
            ApplyRequest::from_profile(&profile, false)
                .validate()
                .is_ok()
        );

        profile.fan_temperature_average_seconds = MAX_FAN_TEMPERATURE_AVERAGE_SECONDS + 1;
        assert!(
            ApplyRequest::from_profile(&profile, false)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn frame_limit_is_one_shared_v3_boundary() {
        assert_eq!(MAX_FRAME_BYTES, 64 * 1024);
        assert_eq!(DaemonEventKind::ALL.len(), 4);
        assert_eq!(
            DaemonEventKind::parse("state-changed"),
            Some(DaemonEventKind::StateChanged)
        );
        assert_eq!(DaemonEventKind::parse("unknown"), None);
    }

    #[test]
    fn controller_capture_and_action_roundtrip() {
        let request = WireRequest {
            version: PROTOCOL_VERSION,
            client_id: ClientId::new(1).unwrap(),
            request_id: RequestId::new(2).unwrap(),
            command: Command::SetControllerCapture { enabled: true },
        };
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.contains("\"cmd\":\"set-controller-capture\""));
        let event = DaemonEvent {
            kind: DaemonEventKind::ControllerAction,
            action: Some(ControllerAction::Accept),
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
            on_battery: Some(true),
        };
        let decoded: DaemonEvent = serde_json::from_str(&serde_json::to_string(&event).unwrap())
            .expect("power-source event should decode");
        assert_eq!(decoded.kind, DaemonEventKind::PowerSourceChanged);
        assert_eq!(decoded.on_battery, Some(true));
    }

    #[test]
    fn command_roundtrips_keep_typed_decodes() {
        let cases = [
            (
                Command::SetBatteryOneTimeCharge { enabled: true },
                "set-battery-one-time-charge",
            ),
            (
                Command::GetFactoryFanCurves {
                    ppd_profiles: vec!["power-saver".into(), "balanced".into()],
                },
                "get-factory-fan-curves",
            ),
            (
                Command::ApplyUndervoltOnce { offset: -20 },
                "apply-undervolt-once",
            ),
        ];
        for (command, name) in cases {
            let request = WireRequest {
                version: PROTOCOL_VERSION,
                client_id: ClientId::new(1).unwrap(),
                request_id: RequestId::new(3).unwrap(),
                command,
            };
            let text = serde_json::to_string(&request).unwrap();
            assert!(text.contains(&format!("\"cmd\":\"{name}\"")));
            let decoded: WireRequest = serde_json::from_str(&text).unwrap();
            match decoded.command {
                Command::SetBatteryOneTimeCharge { enabled } => assert!(enabled),
                Command::GetFactoryFanCurves { ppd_profiles } => assert_eq!(ppd_profiles.len(), 2),
                Command::ApplyUndervoltOnce { offset } => assert_eq!(offset, -20),
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[test]
    fn v3_request_golden_is_strict_and_correlated() {
        let request: WireRequest =
            serde_json::from_str(r#"{"version":3,"client_id":"00000000000000000000000000000001","request_id":42,"cmd":"get-state"}"#).unwrap();
        assert_eq!(request.client_id, ClientId::new(1).unwrap());
        assert_eq!(request.request_id.get(), 42);
        assert!(matches!(request.command, Command::GetState));
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"version":3,"client_id":"00000000000000000000000000000001","request_id":42,"cmd":"get-state"}"#
        );
    }

    #[test]
    fn client_identity_is_fixed_width_nonzero_hex() {
        let client_id = ClientId::new(0xabu128).unwrap();
        assert_eq!(
            serde_json::to_string(&client_id).unwrap(),
            r#""000000000000000000000000000000ab""#
        );
        assert_eq!(
            serde_json::from_str::<ClientId>(r#""000000000000000000000000000000AB""#).unwrap(),
            client_id
        );
        for invalid in [
            r#""00000000000000000000000000000000""#,
            r#""1""#,
            r#""gggggggggggggggggggggggggggggggg""#,
            "1",
        ] {
            assert!(serde_json::from_str::<ClientId>(invalid).is_err());
        }
    }

    #[test]
    fn zero_and_unknown_request_fields_are_rejected() {
        assert!(
            serde_json::from_str::<WireRequest>(
                r#"{"version":3,"client_id":"00000000000000000000000000000001","request_id":0,"cmd":"get-state"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<WireRequest>(
                r#"{"version":3,"client_id":"00000000000000000000000000000001","request_id":1,"cmd":"get-state","future":true}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<WireRequest>(
                r#"{"version":3,"client_id":"00000000000000000000000000000001","request_id":2,"cmd":"set-lighting","device":"keyboard","state":{"enabled":false,"future":true}}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<WireRequest>(
                r#"{"version":3,"client_id":"00000000000000000000000000000001","request_id":3,"cmd":"apply","request":{"ppd_profile":null,"power_limits":null,"fan_mode":"firmware","fan_curves":null,"undervolt":null,"future":true}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn outcome_response_golden_is_versioned() {
        let response = WireResponse::progress(RequestId::new(42).unwrap(), RequestOutcome::Started);
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"version":3,"request_id":42,"outcome":"started","ok":true}"#
        );
    }

    #[test]
    fn unknown_response_fields_and_outcomes_are_rejected() {
        assert!(
            serde_json::from_str::<WireResponse>(
                r#"{"version":3,"request_id":1,"outcome":"future","ok":true}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<WireResponse>(
                r#"{"version":3,"request_id":1,"outcome":"completed","ok":true,"future":true}"#
            )
            .is_err()
        );
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
    fn unknown_ppd_profile_has_no_stock_table_and_is_rejected() {
        assert!(stock_tdp(Some("future-mode")).is_none());
        let mut request =
            ApplyRequest::from_profile(&Profile::builtin("balanced", "Balanced"), false);
        request.ppd_profile = Some("future-mode".into());
        assert!(request.validate().is_err());
    }

    #[test]
    fn direct_fan_mode_requires_curves() {
        let mut request =
            ApplyRequest::from_profile(&Profile::builtin("balanced", "Balanced"), false);
        request.fan_mode = FanControlMode::Direct;
        request.fan_curves = None;
        assert!(request.validate().is_err());
        request.fan_curves = Some(crate::profile::stock_fan_curves(Some("balanced")));
        assert!(request.validate().is_ok());
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
