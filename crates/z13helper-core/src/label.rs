//! Mode header label formatting (G-Helper style).
use crate::profile::{FanControlMode, Profile};

pub fn mode_label(profile: &Profile) -> String {
    let mut s = format!("Mode: {}", profile.name);
    if profile.apply_fan_curve {
        match profile.fan_control_mode {
            FanControlMode::Firmware => s.push('+'),
            FanControlMode::Direct => s.push_str("+EC"),
        }
    }
    if profile.apply_power_limits {
        s.push_str(&format!(" {}W", profile.pl1_spl));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;

    #[test]
    fn mode_labels_cover_builtin_curve_and_power_variants() {
        let mut cases = Vec::new();
        cases.push((Profile::builtin("balanced", "Balanced"), "Mode: Balanced"));
        let mut firmware = Profile::builtin("balanced", "Balanced");
        firmware.apply_fan_curve = true;
        firmware.apply_power_limits = true;
        firmware.pl1_spl = 20;
        cases.push((firmware, "Mode: Balanced+ 20W"));
        let mut direct = Profile::builtin("balanced", "Balanced");
        direct.apply_fan_curve = true;
        direct.fan_control_mode = FanControlMode::Direct;
        cases.push((direct, "Mode: Balanced+EC"));
        for (profile, expected) in cases {
            assert_eq!(mode_label(&profile), expected);
        }
    }
}
