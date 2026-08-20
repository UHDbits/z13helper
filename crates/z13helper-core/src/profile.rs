use serde::{Deserialize, Serialize};

pub type FanCurve = [[i32; 2]; 8];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FanControlMode {
    #[default]
    Firmware,
    Direct,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub builtin: bool,
    pub ppd_profile: Option<String>,
    pub apply_power_limits: bool,
    pub pl1_spl: u32,
    pub pl2_sppt: u32,
    pub fppt: u32,
    pub apply_fan_curve: bool,
    #[serde(default)]
    pub fan_control_mode: FanControlMode,
    #[serde(default = "default_fan_hysteresis")]
    pub fan_hysteresis_up: u8,
    #[serde(default = "default_fan_hysteresis")]
    pub fan_hysteresis_down: u8,
    #[serde(default = "default_fan_temperature_average_seconds")]
    pub fan_temperature_average_seconds: u8,
    #[serde(default = "default_unified_fan_control")]
    pub unified_fan_control: bool,
    pub fan_curves: [FanCurve; 2],
    #[serde(default)]
    pub factory_fan_curves_loaded: bool,
    pub apply_undervolt: bool,
    pub cpu_co: i32,
    #[serde(default = "default_cpu_temp_limit")]
    pub cpu_temp_limit: u8,
}

pub const fn default_fan_hysteresis() -> u8 {
    3
}

pub const fn default_fan_temperature_average_seconds() -> u8 {
    6
}

pub const fn default_unified_fan_control() -> bool {
    true
}

pub const fn default_cpu_temp_limit() -> u8 {
    95
}

fn builtin_ppd(id: &str) -> &'static str {
    match id {
        "silent" => "power-saver",
        "turbo" => "performance",
        _ => "balanced",
    }
}

/// Measured stock PPT values used to initialize the optional custom controls.
pub fn stock_ppt(ppd_profile: Option<&str>) -> (u32, u32, u32) {
    match ppd_profile {
        Some("power-saver") => (40, 55, 55),
        Some("performance") => (70, 86, 86),
        _ => (52, 71, 70),
    }
}

/// G-Helper's current portable fallback curves. These are editor defaults,
/// not a claim about the firmware's private real-time fan control law.
/// G-Helper stores duty as percent; these values are scaled to PWM 0-255.
pub fn stock_fan_curves(ppd_profile: Option<&str>) -> [FanCurve; 2] {
    match ppd_profile {
        Some("power-saver") => [
            // CPU: 0/0/3/12/20/28/34/41 %
            [
                [30, 0],
                [49, 0],
                [59, 7],
                [66, 30],
                [71, 51],
                [80, 71],
                [90, 86],
                [100, 104],
            ],
            // GPU: 0/0/4/17/27/35/40/45 %
            [
                [30, 0],
                [49, 0],
                [59, 10],
                [66, 43],
                [71, 68],
                [80, 89],
                [90, 102],
                [100, 114],
            ],
        ],
        Some("performance") => [
            // CPU: 17/26/34/41/52/67/81/90 %
            [
                [30, 43],
                [63, 66],
                [68, 86],
                [72, 104],
                [76, 132],
                [80, 170],
                [84, 206],
                [98, 229],
            ],
            // GPU: 22/31/38/45/57/71/85/95 %
            [
                [30, 56],
                [63, 79],
                [68, 96],
                [72, 114],
                [76, 145],
                [80, 181],
                [84, 216],
                [98, 242],
            ],
        ],
        _ => [
            // CPU: 8/17/22/26/34/41/48/69 %
            [
                [58, 20],
                [61, 43],
                [64, 56],
                [68, 66],
                [72, 86],
                [77, 104],
                [81, 122],
                [98, 175],
            ],
            // GPU: 12/22/29/31/38/45/52/74 %
            [
                [58, 30],
                [61, 56],
                [64, 73],
                [68, 79],
                [72, 96],
                [77, 114],
                [81, 132],
                [98, 188],
            ],
        ],
    }
}

impl Profile {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("profile name must not be empty".into());
        }
        if let Some(ppd_profile) = &self.ppd_profile
            && ppd_profile.trim().is_empty()
        {
            return Err("PPD profile must not be empty".into());
        }
        for (name, value, maximum) in [
            ("PL1", self.pl1_spl, 93),
            ("PL2", self.pl2_sppt, 93),
            ("FPPT", self.fppt, 120),
        ] {
            if !(5..=maximum).contains(&value) {
                return Err(format!("{name} must be between 5 and {maximum} watts"));
            }
        }
        if self.apply_power_limits && (self.pl2_sppt < self.pl1_spl || self.fppt < self.pl2_sppt) {
            return Err("power limits must satisfy PL1 <= PL2 <= FPPT".into());
        }
        if !(1..=5).contains(&self.fan_hysteresis_up)
            || !(1..=5).contains(&self.fan_hysteresis_down)
        {
            return Err("fan hysteresis values must be between 1 and 5".into());
        }
        if self.fan_temperature_average_seconds > 15 {
            return Err("fan temperature averaging must be between 0 and 15 seconds".into());
        }
        if !(-40..=0).contains(&self.cpu_co) {
            return Err("Curve Optimizer offset must be between -40 and 0".into());
        }
        if !(80..=99).contains(&self.cpu_temp_limit) {
            return Err("APU temperature limit must be between 80 and 99°C".into());
        }
        for curve in &self.fan_curves {
            crate::curve::validate(curve).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    pub fn builtin(id: &str, name: &str) -> Self {
        let ppd_profile = builtin_ppd(id);
        let (pl1, pl2, pl3) = stock_ppt(Some(ppd_profile));
        Self {
            id: id.into(),
            name: name.into(),
            builtin: true,
            ppd_profile: Some(ppd_profile.into()),
            apply_power_limits: false,
            pl1_spl: pl1,
            pl2_sppt: pl2,
            fppt: pl3,
            apply_fan_curve: false,
            fan_control_mode: FanControlMode::Firmware,
            fan_hysteresis_up: default_fan_hysteresis(),
            fan_hysteresis_down: default_fan_hysteresis(),
            fan_temperature_average_seconds: default_fan_temperature_average_seconds(),
            unified_fan_control: default_unified_fan_control(),
            fan_curves: stock_fan_curves(Some(ppd_profile)),
            factory_fan_curves_loaded: false,
            apply_undervolt: false,
            cpu_co: 0,
            cpu_temp_limit: default_cpu_temp_limit(),
        }
    }

    /// Restore the profile's stock PPT preview, fan curves, and undervolt.
    /// Built-ins also restore their original PPD selection.
    pub fn factory_defaults(&mut self) {
        if self.builtin {
            self.ppd_profile = Some(builtin_ppd(&self.id).into());
        }
        let ppd_profile = self.ppd_profile.as_deref();
        let (pl1, pl2, pl3) = stock_ppt(ppd_profile);
        self.apply_power_limits = false;
        self.pl1_spl = pl1;
        self.pl2_sppt = pl2;
        self.fppt = pl3;
        self.apply_fan_curve = false;
        self.fan_control_mode = FanControlMode::Firmware;
        self.fan_hysteresis_up = default_fan_hysteresis();
        self.fan_hysteresis_down = default_fan_hysteresis();
        self.fan_temperature_average_seconds = default_fan_temperature_average_seconds();
        self.unified_fan_control = default_unified_fan_control();
        self.fan_curves = stock_fan_curves(ppd_profile);
        self.factory_fan_curves_loaded = false;
        self.apply_undervolt = false;
        self.cpu_co = 0;
        self.cpu_temp_limit = default_cpu_temp_limit();
    }
}

pub fn default_fan_curve() -> FanCurve {
    stock_fan_curves(Some("balanced"))[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_defaults_restores_builtin_stock() {
        let mut profile = Profile::builtin("silent", "Silent");
        profile.ppd_profile = Some("performance".into());
        profile.apply_power_limits = true;
        profile.pl1_spl = 10;
        profile.apply_fan_curve = true;
        profile.fan_control_mode = FanControlMode::Direct;
        profile.fan_curves[0][0] = [1, 2];
        profile.apply_undervolt = true;
        profile.cpu_co = -20;
        profile.cpu_temp_limit = 80;
        profile.factory_defaults();
        assert_eq!(profile, Profile::builtin("silent", "Silent"));
    }

    #[test]
    fn g_helper_fallbacks_keep_cpu_and_gpu_distinct() {
        for ppd in ["power-saver", "balanced", "performance"] {
            let curves = stock_fan_curves(Some(ppd));
            assert_ne!(curves[0], curves[1]);
        }
    }

    #[test]
    fn factory_apu_temperature_limit_is_95c() {
        let mut profile = Profile::builtin("balanced", "Balanced");
        assert_eq!(profile.cpu_temp_limit, 95);
        profile.cpu_temp_limit = 80;
        profile.factory_defaults();
        assert_eq!(profile.cpu_temp_limit, 95);
    }
}
