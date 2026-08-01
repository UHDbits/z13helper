//! Placeholder — implemented in the config milestone.
use crate::profile::{Base, Profile};
use serde::{Deserialize, Serialize};

pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    pub active_profile: String,
    pub auto_switch_on_power_source: bool,
    pub last_profile_on_ac: String,
    pub last_profile_on_battery: String,
    pub power_source_debounce_ms: u64,
    pub show_hud: bool,
    pub fan_clamp_to_grid: bool,
    pub profiles: Vec<Profile>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            active_profile: "balanced".into(),
            auto_switch_on_power_source: true,
            last_profile_on_ac: "turbo".into(),
            last_profile_on_battery: "silent".into(),
            power_source_debounce_ms: 2000,
            show_hud: true,
            fan_clamp_to_grid: true,
            profiles: vec![
                Profile::builtin("silent", "Silent", Base::Quiet),
                Profile::builtin("balanced", "Balanced", Base::Balanced),
                Profile::builtin("turbo", "Turbo", Base::Performance),
            ],
        }
    }
}
