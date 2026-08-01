use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Request {
    pub cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color2: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brightness: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pl1: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pl2: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pl3: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub state: Option<State>,
    #[serde(default)]
    pub event: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub lighting: LightingState,
    #[serde(default)]
    pub devices: Option<HashMap<String, LightingState>>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub battery_limit: Option<i32>,
    #[serde(default)]
    pub boot_sound: Option<i32>,
    #[serde(default)]
    pub panel_overdrive: Option<i32>,
    #[serde(default)]
    pub fan_curve: Option<FanCurveState>,
    #[serde(default)]
    pub tdp: Option<TdpState>,
    #[serde(default)]
    pub undervolt: Option<UndervoltState>,
    #[serde(default)]
    pub undervolt_available: bool,
    #[serde(default)]
    pub temperature: Option<i32>,
    #[serde(default)]
    pub fan_rpm: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanCurvePoint {
    pub temp: i32,
    pub pwm: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanCurveState {
    pub mode: i32,
    pub points: Vec<FanCurvePoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndervoltState {
    pub cpu_co: i32,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TdpState {
    pub pl1_spl: i32,
    pub pl2_sppt: i32,
    pub fppt: i32,
    #[serde(default)]
    pub apu_sppt: i32,
    #[serde(default)]
    pub platform_sppt: i32,
}
