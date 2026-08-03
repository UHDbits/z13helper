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

    pub fn update(&mut self, now: Instant, temperature_c: i32, seconds: u8) -> i32 {
        if seconds == 0 {
            self.clear();
            return temperature_c;
        }

        self.samples.push_back((now, temperature_c));
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

    #[test]
    fn temperature_average_uses_the_configured_rolling_window() {
        let start = Instant::now();
        let mut average = TemperatureAverager::default();
        assert_eq!(average.update(start, 60, 6), 60);
        assert_eq!(average.update(start + Duration::from_secs(1), 66, 6), 63);
        assert_eq!(average.update(start + Duration::from_secs(2), 72, 6), 66);
        assert_eq!(
            average.update(start + Duration::from_secs(6), 78, 6),
            (66 + 72 + 78) / 3
        );
    }

    #[test]
    fn zero_temperature_average_returns_raw_values_and_clears_history() {
        let start = Instant::now();
        let mut average = TemperatureAverager::default();
        assert_eq!(average.update(start, 60, 6), 60);
        assert_eq!(average.update(start + Duration::from_secs(1), 72, 0), 72);
        assert_eq!(average.update(start + Duration::from_secs(2), 60, 6), 60);
    }
}
