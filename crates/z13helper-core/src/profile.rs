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
    pub fan_curves: [FanCurve; 2],
    #[serde(default)]
    pub factory_fan_curves_loaded: bool,
    pub apply_undervolt: bool,
    pub cpu_co: i32,
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
            fan_curves: stock_fan_curves(Some(ppd_profile)),
            factory_fan_curves_loaded: false,
            apply_undervolt: false,
            cpu_co: 0,
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
        self.fan_curves = stock_fan_curves(ppd_profile);
        self.factory_fan_curves_loaded = false;
        self.apply_undervolt = false;
        self.cpu_co = 0;
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
}
