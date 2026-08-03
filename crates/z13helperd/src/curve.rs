use std::collections::VecDeque;
use std::time::{Duration, Instant};

use z13helper_core::curve::Curve;

#[derive(Debug, Default)]
pub struct TemperatureAverager {
    samples: VecDeque<(Instant, i32)>,
}

impl TemperatureAverager {
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Temperatures are kept in millidegrees C so the controller does not
    /// throw away the resolution supplied by hwmon before interpolation.
    pub fn update(&mut self, now: Instant, temperature_millic: i32, seconds: u8) -> i32 {
        if seconds == 0 {
            self.clear();
            return temperature_millic;
        }

        self.samples.push_back((now, temperature_millic));
        let window = Duration::from_secs(u64::from(seconds));
        while self
            .samples
            .front()
            .is_some_and(|(sampled_at, _)| now.duration_since(*sampled_at) >= window)
        {
            self.samples.pop_front();
        }

        let count = self.samples.len() as i64;
        let sum: i64 = self
            .samples
            .iter()
            .map(|(_, temperature)| i64::from(*temperature))
            .sum();
        if sum >= 0 {
            ((sum + count / 2) / count) as i32
        } else {
            ((sum - count / 2) / count) as i32
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HysteresisState {
    output_temperature_millic: Option<i32>,
    direction: Option<Direction>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Direction {
    Rising,
    Falling,
}

/// Apply directional reversal hysteresis to the temperature used for direct
/// EC interpolation. Continuing in the same direction follows the sensor at
/// its native resolution. The configured deadband is applied only before the
/// direction reverses, avoiding the old repeated 1–5°C staircase.
pub fn hysteretic_temperature(
    temperature_millic: i32,
    up: u8,
    down: u8,
    mut state: HysteresisState,
) -> (i32, HysteresisState) {
    let Some(held) = state.output_temperature_millic else {
        state.output_temperature_millic = Some(temperature_millic);
        return (temperature_millic, state);
    };

    let next_direction = if temperature_millic > held {
        Some(Direction::Rising)
    } else if temperature_millic < held {
        Some(Direction::Falling)
    } else {
        None
    };
    let Some(next_direction) = next_direction else {
        return (held, state);
    };

    let reversing = state
        .direction
        .is_some_and(|direction| direction != next_direction);
    let deadband_millic = match next_direction {
        Direction::Rising => i32::from(up) * 1_000,
        Direction::Falling => i32::from(down) * 1_000,
    };
    if reversing && (temperature_millic - held).unsigned_abs() < deadband_millic as u32 {
        return (held, state);
    }
    state.output_temperature_millic = Some(temperature_millic);
    state.direction = Some(next_direction);
    (temperature_millic, state)
}

pub fn duty_at(curve: &Curve, temperature_millic: i32) -> u8 {
    let first_temperature = curve[0][0] * 1_000;
    let last_temperature = curve[curve.len() - 1][0] * 1_000;
    if temperature_millic <= first_temperature {
        return curve[0][1].clamp(0, 255) as u8;
    }
    if temperature_millic >= last_temperature {
        return curve[curve.len() - 1][1].clamp(0, 255) as u8;
    }
    let pair = curve
        .windows(2)
        .find(|pair| temperature_millic <= pair[1][0] * 1_000)
        .expect("curve endpoints cover the temperature");
    let (x0, y0) = (pair[0][0] * 1_000, pair[0][1]);
    let (x1, y1) = (pair[1][0] * 1_000, pair[1][1]);
    if x1 == x0 {
        return y1.clamp(0, 255) as u8;
    }
    let numerator = (temperature_millic - x0) * (y1 - y0);
    let rounded = if numerator >= 0 {
        numerator + (x1 - x0) / 2
    } else {
        numerator - (x1 - x0) / 2
    };
    (y0 + rounded / (x1 - x0)).clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

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
    fn directional_hysteresis_follows_then_holds_only_a_reversal() {
        let (temperature, state) = hysteretic_temperature(70_000, 3, 2, HysteresisState::default());
        assert_eq!(temperature, 70_000);
        let (temperature, state) = hysteretic_temperature(70_250, 3, 2, state);
        assert_eq!(temperature, 70_250);
        let (temperature, state) = hysteretic_temperature(71_000, 3, 2, state);
        assert_eq!(temperature, 71_000);
        let (temperature, state) = hysteretic_temperature(69_500, 3, 2, state);
        assert_eq!(temperature, 71_000);
        let (temperature, state) = hysteretic_temperature(69_000, 3, 2, state);
        assert_eq!(temperature, 69_000);
        let (temperature, _) = hysteretic_temperature(68_750, 3, 2, state);
        assert_eq!(temperature, 68_750);
    }

    #[test]
    fn temperature_alone_never_overrides_curve() {
        assert_eq!(duty_at(&curve(), 100_000), 180);
        assert_eq!(duty_at(&curve(), 110_000), 180);
    }

    #[test]
    fn interpolation_retains_sub_degree_precision() {
        assert_eq!(duty_at(&curve(), 40_000), 60);
        assert_eq!(duty_at(&curve(), 40_250), 61);
        assert_eq!(duty_at(&curve(), 40_500), 61);
    }

    #[test]
    fn temperature_average_uses_the_configured_rolling_window() {
        let start = Instant::now();
        let mut average = TemperatureAverager::default();
        assert_eq!(average.update(start, 60_125, 6), 60_125);
        assert_eq!(
            average.update(start + Duration::from_secs(1), 66_125, 6),
            63_125
        );
        assert_eq!(
            average.update(start + Duration::from_secs(2), 72_125, 6),
            66_125
        );
        assert_eq!(
            average.update(start + Duration::from_secs(6), 78_125, 6),
            (66_125 + 72_125 + 78_125) / 3
        );
    }

    #[test]
    fn zero_temperature_average_returns_raw_values_and_clears_history() {
        let start = Instant::now();
        let mut average = TemperatureAverager::default();
        assert_eq!(average.update(start, 60_000, 6), 60_000);
        assert_eq!(
            average.update(start + Duration::from_secs(1), 72_000, 0),
            72_000
        );
        assert_eq!(
            average.update(start + Duration::from_secs(2), 60_000, 6),
            60_000
        );
    }
}
