//! Mode header label formatting (G-Helper style).
use crate::profile::Profile;

pub fn mode_label(profile: &Profile) -> String {
    let mut s = format!("Mode: {}", profile.name);
    if profile.apply_fan_curve {
        s.push('+');
    }
    if profile.apply_power_limits {
        s.push_str(&format!(" {}W", profile.pl1_spl));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Base, Profile};

    #[test]
    fn plain_builtin() {
        let p = Profile::builtin("balanced", "Balanced", Base::Balanced);
        assert_eq!(mode_label(&p), "Mode: Balanced");
    }

    #[test]
    fn with_curve_and_power() {
        let mut p = Profile::builtin("balanced", "Balanced", Base::Balanced);
        p.apply_fan_curve = true;
        p.apply_power_limits = true;
        p.pl1_spl = 20;
        assert_eq!(mode_label(&p), "Mode: Balanced+ 20W");
    }
}
