//! Fan curve authoring math. Runtime safety floors are daemon-owned.
//!
//! Fan-curve validation and editing constraints.

use thiserror::Error;

pub const POINT_COUNT: usize = 8;
pub const TEMP_MIN: i32 = 20;
pub const TEMP_MAX: i32 = 110;
pub const PWM_MIN: i32 = 0;
pub const PWM_MAX: i32 = 255;
pub const TDP_MAX_SAFE: u32 = 75;

pub type Curve = [[i32; 2]; POINT_COUNT]; // [temp, pwm]

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CurveError {
    #[error("fan curve must have exactly {POINT_COUNT} points")]
    BadLength,
    #[error("temp {temp} in point {index} out of range 0–120")]
    TempRange { temp: i32, index: usize },
    #[error("pwm {pwm} in point {index} out of range 0–255")]
    PwmRange { pwm: i32, index: usize },
    #[error("temps must be non-decreasing")]
    TempNotIncreasing,
    #[error("pwm values must be non-decreasing")]
    PwmDecreasing,
}

/// Validate a curve against the daemon's rules (without the high-TDP floor).
pub fn validate(curve: &Curve) -> Result<(), CurveError> {
    // ASUS factory tables may repeat a temperature breakpoint (Silent does at
    // 71°C), so only a backwards step is invalid here. Editor normalization
    // still keeps newly authored curves strictly increasing.
    for (i, pt) in curve.iter().enumerate() {
        let (temp, pwm) = (pt[0], pt[1]);
        if !(0..=120).contains(&temp) {
            return Err(CurveError::TempRange { temp, index: i + 1 });
        }
        if !(0..=255).contains(&pwm) {
            return Err(CurveError::PwmRange { pwm, index: i + 1 });
        }
        if i > 0 {
            if temp < curve[i - 1][0] {
                return Err(CurveError::TempNotIncreasing);
            }
            if pwm < curve[i - 1][1] {
                return Err(CurveError::PwmDecreasing);
            }
        }
    }
    Ok(())
}

/// Enforce monotonicity and bounds after dragging point `idx`.
///
/// Clamp the edited point and push neighbours so
/// temps strictly increase and PWMs are non-decreasing, keep index-based
/// bounds so points cannot collapse onto an edge.
pub fn enforce_curve(curve: &mut Curve, idx: usize) {
    if idx >= POINT_COUNT {
        return;
    }

    let clamp_point = |p: &mut [i32; 2], min_pwm: i32| {
        p[0] = p[0].clamp(TEMP_MIN, TEMP_MAX);
        p[1] = p[1].clamp(min_pwm, PWM_MAX);
    };

    clamp_point(&mut curve[idx], PWM_MIN);

    // Index-based temp bounds leave room for neighbours.
    let lo = TEMP_MIN + idx as i32;
    let hi = TEMP_MAX - (POINT_COUNT as i32 - 1 - idx as i32);
    curve[idx][0] = curve[idx][0].clamp(lo, hi);

    // Forward cascade: temps strictly increasing, PWM non-decreasing.
    for i in idx + 1..POINT_COUNT {
        if curve[i][0] <= curve[i - 1][0] {
            curve[i][0] = (curve[i - 1][0] + 1).min(TEMP_MAX);
        }
        if curve[i][1] < curve[i - 1][1] {
            curve[i][1] = curve[i - 1][1];
        }
        clamp_point(&mut curve[i], PWM_MIN);
    }

    // Backward cascade.
    for i in (0..idx).rev() {
        if curve[i][0] >= curve[i + 1][0] {
            curve[i][0] = (curve[i + 1][0] - 1).max(TEMP_MIN);
        }
        if curve[i][1] > curve[i + 1][1] {
            curve[i][1] = curve[i + 1][1];
        }
        clamp_point(&mut curve[i], PWM_MIN);
    }

    // Final pass: re-clamp everything and fix any residual collisions.
    for p in curve.iter_mut() {
        clamp_point(p, PWM_MIN);
    }
    for i in 1..POINT_COUNT {
        if curve[i][0] <= curve[i - 1][0] {
            curve[i][0] = (curve[i - 1][0] + 1).min(TEMP_MAX);
        }
        if curve[i][1] < curve[i - 1][1] {
            curve[i][1] = curve[i - 1][1];
        }
    }
}

/// Shift every point's PWM by `delta` across the full authoring range.
pub fn shift_curve_vertical(curve: &mut Curve, delta: i32) {
    for pt in curve.iter_mut() {
        pt[1] = (pt[1] + delta).clamp(PWM_MIN, PWM_MAX);
    }
}

/// Convert PWM 0–255 to a display percentage 0–100.
pub fn pwm_to_percent(pwm: i32) -> i32 {
    ((pwm as f64) * 100.0 / 255.0).round() as i32
}

/// Convert a display percentage 0–100 to PWM 0–255.
pub fn percent_to_pwm(pct: i32) -> i32 {
    (pct.clamp(0, 100) * 255) / 100
}

/// Format a curve for the daemon wire protocol.
pub fn to_wire(curve: &Curve) -> String {
    curve
        .iter()
        .map(|p| format!("{}:{}", p[0], p[1]))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::default_fan_curve;

    #[test]
    fn default_curve_valid() {
        let c = default_fan_curve();
        validate(&c).unwrap();
    }

    #[test]
    fn firmware_table_may_repeat_a_temperature_breakpoint() {
        let mut curve = default_fan_curve();
        curve[7] = curve[6];
        validate(&curve).unwrap();
    }

    #[test]
    fn enforce_pushes_neighbours() {
        let mut c = default_fan_curve();
        // Drag point 3's temp above point 4's.
        c[3][0] = 70;
        enforce_curve(&mut c, 3);
        for i in 1..POINT_COUNT {
            assert!(c[i][0] > c[i - 1][0], "temp not increasing at {i}: {:?}", c);
            assert!(c[i][1] >= c[i - 1][1], "pwm decreasing at {i}: {:?}", c);
        }
    }

    #[test]
    fn enforce_does_not_apply_runtime_floor() {
        let mut c = default_fan_curve();
        c[0][1] = 0;
        enforce_curve(&mut c, 0);
        assert_eq!(c[0][1], 0);
    }

    #[test]
    fn no_edge_collapse() {
        let mut c = default_fan_curve();
        // Drag first point to far left.
        c[0][0] = TEMP_MIN;
        enforce_curve(&mut c, 0);
        // All temps must remain distinct.
        let mut seen = std::collections::HashSet::new();
        for pt in &c {
            assert!(seen.insert(pt[0]), "duplicate temp {}", pt[0]);
        }
    }

    #[test]
    fn shift_vertical() {
        let mut c = [[50, 100]; 8];
        for (i, pt) in c.iter_mut().enumerate() {
            pt[0] = 40 + i as i32 * 5;
        }
        shift_curve_vertical(&mut c, 50);
        assert_eq!(c[0][1], 150);
        shift_curve_vertical(&mut c, 200);
        assert_eq!(c[0][1], PWM_MAX);
    }

    #[test]
    fn pwm_percent_roundtrip() {
        assert_eq!(pwm_to_percent(0), 0);
        assert_eq!(pwm_to_percent(255), 100);
        assert_eq!(pwm_to_percent(204), 80);
        assert_eq!(percent_to_pwm(80), 204);
    }

    #[test]
    fn to_wire_format() {
        let c = default_fan_curve();
        let s = to_wire(&c);
        assert!(s.starts_with("58:20,61:43"));
        assert_eq!(s.split(',').count(), 8);
    }
}
