use z13helper_core::curve::{Curve, TDP_MAX_SAFE};
use z13helper_core::protocol::FanFloorConfig;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FloorGate {
    pub engaged: bool,
    pub engaged_since_ms: Option<u64>,
}

pub fn controlled_duty(
    curve: &Curve,
    temperature_c: i32,
    pl1_w: Option<u32>,
    config: FanFloorConfig,
    mut gate: FloorGate,
    now_ms: u64,
) -> (u8, FloorGate) {
    let high_power = pl1_w.is_some_and(|pl1| pl1 > TDP_MAX_SAFE);
    if !high_power {
        gate = FloorGate::default();
    } else if !gate.engaged && temperature_c >= config.engage_temp_c {
        gate.engaged = true;
        gate.engaged_since_ms = Some(now_ms);
    } else if gate.engaged && temperature_c <= config.release_temp_c {
        let dwell_elapsed = gate
            .engaged_since_ms
            .is_some_and(|since| now_ms.saturating_sub(since) >= config.dwell_ms);
        if dwell_elapsed {
            gate = FloorGate::default();
        }
    }
    let duty = duty_at(curve, temperature_c);
    if gate.engaged {
        (duty.max(config.duty), gate)
    } else {
        (duty, gate)
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
    let numerator = (temperature_c - x0) * (y1 - y0);
    let rounded = if numerator >= 0 {
        numerator + (x1 - x0) / 2
    } else {
        numerator - (x1 - x0) / 2
    };
    (y0 + rounded / (x1 - x0)).clamp(0, 255) as u8
}

pub fn firmware_curve(authored: &Curve, pl1_w: u32, config: FanFloorConfig) -> Curve {
    let mut written = *authored;
    if pl1_w > TDP_MAX_SAFE {
        for point in &mut written {
            if point[0] >= config.engage_temp_c {
                point[1] = point[1].max(i32::from(config.duty));
            }
        }
        for index in 1..written.len() {
            written[index][1] = written[index][1].max(written[index - 1][1]);
        }
    }
    written
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
    fn gate_has_hysteresis_and_dwell() {
        let config = FanFloorConfig::default();
        let (duty, gate) = controlled_duty(&curve(), 70, Some(80), config, FloorGate::default(), 0);
        assert_eq!(duty, 204);
        assert!(gate.engaged);
        let (duty, gate) = controlled_duty(&curve(), 64, Some(80), config, gate, 4_000);
        assert_eq!(duty, 204);
        assert!(gate.engaged);
        let (_, gate) = controlled_duty(&curve(), 64, Some(80), config, gate, 5_000);
        assert!(!gate.engaged);
    }

    #[test]
    fn gate_releases_at_configured_boundary_after_dwell() {
        let config = FanFloorConfig::default();
        let (_, gate) = controlled_duty(&curve(), 70, Some(80), config, FloorGate::default(), 0);
        let (_, gate) = controlled_duty(&curve(), 65, Some(80), config, gate, 5_000);
        assert!(!gate.engaged);
    }

    #[test]
    fn temperature_alone_never_overrides_curve() {
        let (duty, gate) = controlled_duty(
            &curve(),
            100,
            Some(75),
            FanFloorConfig::default(),
            FloorGate::default(),
            0,
        );
        assert_eq!(duty, 180);
        assert!(!gate.engaged);
    }

    #[test]
    fn firmware_transform_does_not_mutate_authored_curve() {
        let authored = curve();
        let written = firmware_curve(&authored, 80, FanFloorConfig::default());
        assert_eq!(authored[4][1], 120);
        assert_eq!(written[4][1], 204);
        assert_eq!(written[3][1], 100);
    }
}
