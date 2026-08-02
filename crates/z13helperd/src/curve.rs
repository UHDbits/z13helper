use z13helper_core::curve::Curve;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HysteresisState {
    held_temperature_c: Option<i32>,
}

/// Apply independent rising/falling deadbands to the temperature used for
/// direct EC interpolation. The 1–5 values mirror G-Helper's exposed levels.
pub fn hysteretic_temperature(
    temperature_c: i32,
    up: u8,
    down: u8,
    mut state: HysteresisState,
) -> (i32, HysteresisState) {
    let held = state.held_temperature_c.unwrap_or(temperature_c);
    if temperature_c >= held + i32::from(up) || temperature_c <= held - i32::from(down) {
        state.held_temperature_c = Some(temperature_c);
        (temperature_c, state)
    } else {
        state.held_temperature_c = Some(held);
        (held, state)
    }
}

pub fn duty_at(curve: &Curve, temperature_c: i32) -> u8 {
    if temperature_c <= curve[0][0] {
        return curve[0][1].clamp(0, 255) as u8;
    }
    if temperature_c >= curve[curve.len() - 1][0] {
        return curve[curve.len() - 1][1].clamp(0, 255) as u8;
    }
    let pair = curve
        .windows(2)
        .find(|pair| temperature_c <= pair[1][0])
        .expect("curve endpoints cover the temperature");
    let (x0, y0) = (pair[0][0], pair[0][1]);
    let (x1, y1) = (pair[1][0], pair[1][1]);
    if x1 == x0 {
        return y1.clamp(0, 255) as u8;
    }
    let numerator = (temperature_c - x0) * (y1 - y0);
    let rounded = if numerator >= 0 {
        numerator + (x1 - x0) / 2
    } else {
        numerator - (x1 - x0) / 2
    };
    (y0 + rounded / (x1 - x0)).clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve() -> Curve {
        [
            [30, 40],
            [40, 60],
            [50, 80],
            [60, 100],
            [70, 120],
            [80, 140],
            [90, 160],
            [100, 180],
        ]
    }

    #[test]
    fn directional_hysteresis_holds_then_updates_temperature() {
        let (temperature, state) = hysteretic_temperature(70, 3, 3, HysteresisState::default());
        assert_eq!(temperature, 70);
        let (temperature, state) = hysteretic_temperature(72, 3, 3, state);
        assert_eq!(temperature, 70);
        let (temperature, state) = hysteretic_temperature(73, 3, 3, state);
        assert_eq!(temperature, 73);
        let (temperature, _) = hysteretic_temperature(70, 3, 2, state);
        assert_eq!(temperature, 70);
    }

    #[test]
    fn temperature_alone_never_overrides_curve() {
        assert_eq!(duty_at(&curve(), 100), 180);
    }
}
