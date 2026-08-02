use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Base {
    Quiet,
    #[default]
    Balanced,
    Performance,
}

impl Base {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quiet => "quiet",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Quiet => "Silent",
            Self::Balanced => "Balanced",
            Self::Performance => "Turbo",
        }
    }

    /// Short label for the Power Profile dropdown (no PPD suffix).
    pub fn short_label(self) -> &'static str {
        match self {
            Self::Quiet => "Quiet",
            Self::Balanced => "Balanced",
            Self::Performance => "Performance",
        }
    }

    pub fn ppd_label(self) -> &'static str {
        match self {
            Self::Quiet => "Quiet — PPD: power-saver",
            Self::Balanced => "Balanced — PPD: balanced",
            Self::Performance => "Performance — PPD: performance",
        }
    }

    pub fn accent(self) -> &'static str {
        match self {
            Self::Quiet => "#06B48A",
            Self::Balanced => "#3AAEEF",
            Self::Performance => "#FF2020",
        }
    }

    pub fn stock_ppt(self) -> (u32, u32, u32) {
        match self {
            Self::Quiet => (40, 55, 55),
            Self::Balanced => (52, 71, 70),
            Self::Performance => (70, 86, 86),
        }
    }

    /// Default fan curves adapted from G-Helper's CPU presets for each BIOS
    /// base. Firmware auto mode does not expose distinct readable curves via
    /// sysfs (points stay unchanged while `pwm_enable=2`), so these are the
    /// best portable defaults for Silent / Balanced / Turbo.
    ///
    /// G-Helper stores fan levels as 0–100; we convert to PWM 0–255.
    pub fn stock_fan_curve(self) -> [[i32; 2]; 8] {
        match self {
            // G-Helper Silent: 30/49/59/66/71/80/90/100 °C × 0/0/3/12/20/28/34/41 %
            Self::Quiet => [
                [30, 0],
                [49, 0],
                [59, 7],
                [66, 30],
                [71, 51],
                [80, 71],
                [90, 86],
                [100, 104],
            ],
            // G-Helper Balanced: 58/61/64/68/72/77/81/98 °C × 8/17/22/26/34/41/48/69 %
            Self::Balanced => [
                [58, 20],
                [61, 43],
                [64, 56],
                [68, 66],
                [72, 86],
                [77, 104],
                [81, 122],
                [98, 175],
            ],
            // G-Helper Turbo: 30/63/68/72/76/80/84/98 °C × 17/26/34/41/52/67/81/90 %
            Self::Performance => [
                [30, 43],
                [63, 66],
                [68, 86],
                [72, 104],
                [76, 132],
                [80, 170],
                [84, 206],
                [98, 229],
            ],
        }
    }

    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "quiet" | "low-power" | "silent" => Some(Self::Quiet),
            "balanced" => Some(Self::Balanced),
            "performance" | "turbo" => Some(Self::Performance),
            _ => None,
        }
    }
}

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
    pub base: Base,
    pub ppd_profile: Option<String>,
    pub apply_power_limits: bool,
    pub pl1_spl: u32,
    pub pl2_sppt: u32,
    pub fppt: u32,
    pub apply_fan_curve: bool,
    #[serde(default)]
    pub fan_control_mode: FanControlMode,
    pub fan_curves: [[[i32; 2]; 8]; 2],
    pub apply_undervolt: bool,
    pub cpu_co: i32,
}

impl Profile {
    pub fn builtin(id: &str, name: &str, base: Base) -> Self {
        let (pl1, pl2, pl3) = base.stock_ppt();
        Self {
            id: id.into(),
            name: name.into(),
            builtin: true,
            base,
            ppd_profile: Some(
                match base {
                    Base::Quiet => "power-saver",
                    Base::Balanced => "balanced",
                    Base::Performance => "performance",
                }
                .into(),
            ),
            apply_power_limits: false,
            pl1_spl: pl1,
            pl2_sppt: pl2,
            fppt: pl3,
            apply_fan_curve: false,
            fan_control_mode: FanControlMode::Firmware,
            fan_curves: [base.stock_fan_curve(), base.stock_fan_curve()],
            apply_undervolt: false,
            cpu_co: 0,
        }
    }

    /// Restore stock PPT, fan curve, and undervolt for this profile's base.
    /// Built-ins also restore their original Silent/Balanced/Turbo base.
    pub fn factory_defaults(&mut self) {
        if self.builtin {
            let base = match self.id.as_str() {
                "silent" => Base::Quiet,
                "turbo" => Base::Performance,
                _ => Base::Balanced,
            };
            self.base = base;
        }
        let (pl1, pl2, pl3) = self.base.stock_ppt();
        self.apply_power_limits = false;
        self.pl1_spl = pl1;
        self.pl2_sppt = pl2;
        self.fppt = pl3;
        self.apply_fan_curve = false;
        self.fan_control_mode = FanControlMode::Firmware;
        self.fan_curves = [self.base.stock_fan_curve(), self.base.stock_fan_curve()];
        self.apply_undervolt = false;
        self.cpu_co = 0;
    }
}

/// Generic example curve (z13ctl docs). Prefer [`Base::stock_fan_curve`].
pub fn default_fan_curve() -> [[i32; 2]; 8] {
    Base::Balanced.stock_fan_curve()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_defaults_restores_builtin_stock() {
        let mut p = Profile::builtin("silent", "Silent", Base::Quiet);
        p.base = Base::Performance;
        p.apply_power_limits = true;
        p.pl1_spl = 10;
        p.apply_fan_curve = true;
        p.fan_control_mode = FanControlMode::Direct;
        p.fan_curves[0][0] = [1, 2];
        p.apply_undervolt = true;
        p.cpu_co = -20;
        p.factory_defaults();
        let stock = Profile::builtin("silent", "Silent", Base::Quiet);
        assert_eq!(p, stock);
    }
}
