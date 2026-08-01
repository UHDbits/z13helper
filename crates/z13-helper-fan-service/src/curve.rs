use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const POINT_COUNT: usize = 8;
pub const HIGH_POWER_THRESHOLD_W: u32 = 75;
pub const HIGH_POWER_PWM_FLOOR: u8 = 204;
pub const PANIC_TEMPERATURE_C: i32 = 96;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Point {
    pub temperature_c: i32,
    pub duty: u8,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "Vec<Point>", into = "Vec<Point>")]
pub struct Curve([Point; POINT_COUNT]);

#[derive(Debug, Error, Eq, PartialEq)]
pub enum CurveError {
    #[error("fan curve must contain exactly eight points")]
    PointCount,
    #[error("fan curve temperatures must be strictly increasing")]
    TemperatureOrder,
}

impl TryFrom<Vec<Point>> for Curve {
    type Error = CurveError;

    fn try_from(points: Vec<Point>) -> Result<Self, Self::Error> {
        let points: [Point; POINT_COUNT] = points.try_into().map_err(|_| CurveError::PointCount)?;
        if points
            .windows(2)
            .any(|pair| pair[0].temperature_c >= pair[1].temperature_c)
        {
            return Err(CurveError::TemperatureOrder);
        }
        Ok(Self(points))
    }
}

impl From<Curve> for Vec<Point> {
    fn from(curve: Curve) -> Self {
        curve.0.to_vec()
    }
}

impl Curve {
    pub fn duty_at(&self, temperature_c: i32) -> u8 {
        if temperature_c <= self.0[0].temperature_c {
            return self.0[0].duty;
        }
        if temperature_c >= self.0[POINT_COUNT - 1].temperature_c {
            return self.0[POINT_COUNT - 1].duty;
        }

        let pair = self
            .0
            .windows(2)
            .find(|pair| temperature_c <= pair[1].temperature_c)
            .expect("curve endpoints cover the temperature");
        let x0 = pair[0].temperature_c;
        let x1 = pair[1].temperature_c;
        let y0 = i32::from(pair[0].duty);
        let y1 = i32::from(pair[1].duty);
        let numerator = (temperature_c - x0) * (y1 - y0);
        let rounded = if numerator >= 0 {
            numerator + (x1 - x0) / 2
        } else {
            numerator - (x1 - x0) / 2
        };
        (y0 + rounded / (x1 - x0)).clamp(0, 255) as u8
    }
}

pub fn controlled_duty(curve: &Curve, temperature_c: i32, pl1_w: Option<u32>) -> u8 {
    if temperature_c >= PANIC_TEMPERATURE_C {
        return u8::MAX;
    }
    let duty = curve.duty_at(temperature_c);
    if pl1_w.is_some_and(|pl1| pl1 > HIGH_POWER_THRESHOLD_W) {
        duty.max(HIGH_POWER_PWM_FLOOR)
    } else {
        duty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve() -> Curve {
        (0..POINT_COUNT)
            .map(|index| Point {
                temperature_c: 30 + index as i32 * 10,
                duty: 40 + index as u8 * 20,
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap()
    }

    #[test]
    fn interpolates_and_clamps_to_endpoints() {
        let curve = curve();
        assert_eq!(curve.duty_at(20), 40);
        assert_eq!(curve.duty_at(35), 50);
        assert_eq!(curve.duty_at(100), 180);
    }

    #[test]
    fn rejects_wrong_count_and_unsorted_points() {
        assert_eq!(Curve::try_from(vec![]).unwrap_err(), CurveError::PointCount);
        let mut points: Vec<Point> = curve().into();
        points[2].temperature_c = points[1].temperature_c;
        assert_eq!(
            Curve::try_from(points).unwrap_err(),
            CurveError::TemperatureOrder
        );
    }

    #[test]
    fn applies_power_floor_only_above_75_watts() {
        let curve = curve();
        assert_eq!(controlled_duty(&curve, 30, Some(75)), 40);
        assert_eq!(controlled_duty(&curve, 30, Some(76)), 204);
    }

    #[test]
    fn panic_temperature_always_runs_full_speed() {
        assert_eq!(controlled_duty(&curve(), 96, None), 255);
    }
}
