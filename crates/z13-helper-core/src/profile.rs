use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Base {
    Quiet,
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

    pub fn ppd_label(self) -> &'static str {
        match self {
            Self::Quiet => "Quiet — PPD: power-saver",
            Self::Balanced => "Balanced — PPD: balanced",
            Self::Performance => "Performance — PPD: performance",
        }
    }

    pub fn stock_ppt(self) -> (u32, u32, u32) {
        match self {
            Self::Quiet => (40, 55, 55),
            Self::Balanced => (52, 71, 70),
            Self::Performance => (70, 86, 86),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub builtin: bool,
    pub base: Base,
    pub apply_power_limits: bool,
    pub pl1_spl: u32,
    pub pl2_sppt: u32,
    pub fppt: u32,
    pub apply_fan_curve: bool,
    pub fan_curve: [[i32; 2]; 8],
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
            apply_power_limits: false,
            pl1_spl: pl1,
            pl2_sppt: pl2,
            fppt: pl3,
            apply_fan_curve: false,
            fan_curve: default_fan_curve(),
            apply_undervolt: false,
            cpu_co: 0,
        }
    }
}

pub fn default_fan_curve() -> [[i32; 2]; 8] {
    [
        [48, 2],
        [53, 22],
        [57, 30],
        [60, 43],
        [63, 56],
        [65, 68],
        [70, 89],
        [76, 102],
    ]
}
