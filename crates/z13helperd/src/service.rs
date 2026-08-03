use z13helper_core::curve::Curve;
use z13helper_core::protocol::{FanHysteresis, ProbeReply};

use std::time::{Duration, Instant};

use crate::curve::{duty_at, hysteretic_temperature, HysteresisState, TemperatureAverager};
use crate::ec::{EcMailbox, PortIo};

const MODEL: &str = "GZ302EA";
const MAX_CONSECUTIVE_EC_ERRORS: u32 = 3;

const RISE_PWM_PER_SECOND: u32 = 255;
const FALL_PWM_PER_SECOND: u32 = 102;
pub const DIRECT_TICK_INTERVAL: Duration = Duration::from_millis(250);

pub struct Controller<P> {
    ec: EcMailbox<P>,
    curves: Option<[Curve; 2]>,
    hysteresis: FanHysteresis,
    temperature_average_seconds: u8,
    temperature_average: TemperatureAverager,
    hysteresis_state: HysteresisState,
    last_duty: [u8; 2],
    last_ramp_at: Option<Instant>,
    consecutive_ec_errors: u32,
}

impl<P: PortIo> Controller<P> {
    pub fn new(ec: EcMailbox<P>) -> Self {
        Self {
            ec,
            curves: None,
            hysteresis: FanHysteresis::default(),
            temperature_average_seconds:
                z13helper_core::profile::default_fan_temperature_average_seconds(),
            temperature_average: TemperatureAverager::default(),
            hysteresis_state: HysteresisState::default(),
            last_duty: [0; 2],
            last_ramp_at: None,
            consecutive_ec_errors: 0,
        }
    }

    pub fn startup_release_and_probe(&mut self) -> Result<ProbeReply, String> {
        self.release()?;
        let probe = self.ec.probe().map_err(|error| error.to_string())?;
        Ok(ProbeReply {
            model: MODEL.into(),
            ec_version: probe.version,
            fan_count: probe.fan_count,
        })
    }

    pub fn probe(&mut self) -> Result<ProbeReply, String> {
        let probe = self.ec.probe().map_err(|error| error.to_string())?;
        Ok(ProbeReply {
            model: MODEL.into(),
            ec_version: probe.version,
            fan_count: probe.fan_count,
        })
    }

    pub fn enable(
        &mut self,
        curves: [Curve; 2],
        hysteresis: FanHysteresis,
        temperature_average_seconds: u8,
    ) -> Result<(), String> {
        self.ec
            .set_global_mode(true)
            .map_err(|error| self.note_ec_error(error.to_string()))?;
        self.curves = Some(curves);
        self.hysteresis = hysteresis;
        self.temperature_average_seconds = temperature_average_seconds;
        self.temperature_average.clear();
        self.hysteresis_state = HysteresisState::default();
        self.last_duty = [0; 2];
        self.last_ramp_at = None;
        self.consecutive_ec_errors = 0;
        Ok(())
    }

    pub fn release(&mut self) -> Result<(), String> {
        self.curves = None;
        self.temperature_average.clear();
        self.hysteresis_state = HysteresisState::default();
        self.last_ramp_at = None;
        match self.ec.set_global_mode(false) {
            Ok(()) => {
                self.last_duty = [0; 2];
                self.consecutive_ec_errors = 0;
                Ok(())
            }
            Err(error) => Err(self.note_ec_error(error.to_string())),
        }
    }

    /// Advance direct control from the shared, already-sampled temperature.
    /// Sensor acquisition intentionally lives outside this type so telemetry
    /// and the control loop cannot observe different k10temp readings.
    pub fn tick(&mut self, now: Instant, temperature_millic: i32) -> Result<(), String> {
        self.tick_inner(now, temperature_millic, false)
    }

    /// Immediately establish the curve target when direct control is freshly
    /// installed and has no trustworthy prior commanded duty. This also serves
    /// as the prerequisite for a high-power increase. Subsequent thermal
    /// changes still use the normal asymmetric ramp.
    pub fn prime(&mut self, now: Instant, temperature_millic: i32) -> Result<(), String> {
        self.tick_inner(now, temperature_millic, true)
    }

    fn tick_inner(
        &mut self,
        now: Instant,
        temperature_millic: i32,
        force_target: bool,
    ) -> Result<(), String> {
        let Some(curves) = self.curves else {
            return Ok(());
        };
        let averaged_temperature = self.temperature_average.update(
            now,
            temperature_millic,
            self.temperature_average_seconds,
        );
        let (temperature, hysteresis_state) = hysteretic_temperature(
            averaged_temperature,
            self.hysteresis.up,
            self.hysteresis.down,
            self.hysteresis_state,
        );
        self.hysteresis_state = hysteresis_state;
        let targets = [
            duty_at(&curves[0], temperature),
            duty_at(&curves[1], temperature),
        ];
        let elapsed = self
            .last_ramp_at
            .map(|last| now.saturating_duration_since(last))
            .unwrap_or(DIRECT_TICK_INTERVAL);
        let duties = if force_target {
            targets
        } else {
            [
                ramp_duty(self.last_duty[0], targets[0], elapsed),
                ramp_duty(self.last_duty[1], targets[1], elapsed),
            ]
        };
        for (fan, duty) in duties.into_iter().enumerate() {
            // Mailbox writes are expensive and reselect the fan; leave a
            // stable duty untouched rather than sending the same command four
            // times per second.
            if duty == self.last_duty[fan] {
                continue;
            }
            if let Err(error) = self.ec.set_duty(fan as u8, duty) {
                return Err(self.note_ec_error(error.to_string()));
            }
        }
        self.last_duty = duties;
        self.last_ramp_at = Some(now);
        self.consecutive_ec_errors = 0;
        Ok(())
    }

    pub fn sensor_failed(&mut self, error: String) -> String {
        self.release_best_effort();
        format!("sensor failure; EC control released: {error}")
    }

    pub fn direct_enabled(&self) -> bool {
        self.curves.is_some()
    }

    pub fn last_duty(&self) -> [u8; 2] {
        self.last_duty
    }

    fn note_ec_error(&mut self, error: String) -> String {
        self.consecutive_ec_errors = self.consecutive_ec_errors.saturating_add(1);
        if self.consecutive_ec_errors >= MAX_CONSECUTIVE_EC_ERRORS {
            self.release_best_effort();
            format!("repeated EC failure; control released: {error}")
        } else {
            format!("EC failure: {error}")
        }
    }

    fn release_best_effort(&mut self) {
        self.curves = None;
        self.temperature_average.clear();
        self.hysteresis_state = HysteresisState::default();
        self.last_duty = [0; 2];
        self.last_ramp_at = None;
        if let Err(error) = self.ec.set_global_mode(false) {
            tracing::error!(%error, "failed to release EC automatic mode");
        }
    }

    #[cfg(test)]
    fn into_io(self) -> P {
        self.ec.into_inner()
    }
}

/// Move towards the curve target without passing it. A full 0→255 rise takes
/// at most one second; a full fall takes about 2.5 seconds. This output-stage
/// smoothing never creates a target independent of the authored curve.
pub fn ramp_duty(current: u8, target: u8, elapsed: Duration) -> u8 {
    if current == target {
        return target;
    }
    let rate = if target > current {
        RISE_PWM_PER_SECOND
    } else {
        FALL_PWM_PER_SECOND
    };
    let milliseconds = elapsed.as_millis().min(u128::from(u32::MAX)) as u32;
    let step = (milliseconds.saturating_mul(rate).saturating_add(999) / 1_000).max(1);
    if target > current {
        current
            .saturating_add(step.min(u32::from(u8::MAX)) as u8)
            .min(target)
    } else {
        current
            .saturating_sub(step.min(u32::from(u8::MAX)) as u8)
            .max(target)
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::ec::{PortIo, Register, COMMAND_STATUS_PORT, DATA_PORT};
    use z13helper_core::curve::Curve;

    #[derive(Default)]
    struct FakeIo {
        writes: Vec<(u16, u8)>,
    }

    impl PortIo for FakeIo {
        fn read_u8(&mut self, _port: u16) -> io::Result<u8> {
            Ok(0)
        }

        fn write_u8(&mut self, port: u16, value: u8) -> io::Result<()> {
            self.writes.push((port, value));
            Ok(())
        }
    }

    fn curve(duty: i32) -> Curve {
        [
            [20, duty],
            [30, duty],
            [40, duty],
            [50, duty],
            [60, duty],
            [70, duty],
            [80, duty],
            [90, duty],
        ]
    }

    #[test]
    fn ramp_reaches_a_higher_target_within_one_second_without_overshoot() {
        let mut duty = 0;
        let target = 200;
        for _ in 0..4 {
            duty = ramp_duty(duty, target, DIRECT_TICK_INTERVAL);
            assert!(duty <= target);
        }
        assert_eq!(duty, target);
    }

    #[test]
    fn ramp_falls_smoothly_in_about_two_and_a_half_seconds_without_undershoot() {
        let mut duty = 255;
        let target = 20;
        for _ in 0..9 {
            duty = ramp_duty(duty, target, DIRECT_TICK_INTERVAL);
            assert!(duty >= target);
        }
        assert!(duty > target);
        duty = ramp_duty(duty, target, DIRECT_TICK_INTERVAL);
        assert_eq!(duty, target);
    }

    #[test]
    fn controller_prime_establishes_the_curve_target_without_ramping_from_zero() {
        let mut controller = Controller::new(EcMailbox::new(FakeIo::default()));
        controller
            .enable([curve(200), curve(180)], FanHysteresis::default(), 0)
            .unwrap();
        controller.prime(Instant::now(), 60_000).unwrap();
        assert_eq!(controller.last_duty(), [200, 180]);
    }

    #[test]
    fn controller_skips_redundant_duty_writes_and_releases_on_sensor_failure() {
        let mut controller = Controller::new(EcMailbox::new(FakeIo::default()));
        controller
            .enable([curve(100), curve(100)], FanHysteresis::default(), 0)
            .unwrap();
        let now = Instant::now();
        controller.tick(now, 60_000).unwrap();
        controller.tick(now + DIRECT_TICK_INTERVAL, 60_000).unwrap();
        controller
            .tick(now + DIRECT_TICK_INTERVAL * 2, 60_000)
            .unwrap();
        let io = controller.into_io();
        let duty_transactions = io
            .writes
            .windows(5)
            .filter(|writes| {
                writes[0] == (COMMAND_STATUS_PORT, 0xff)
                    && writes[1] == (COMMAND_STATUS_PORT, 0xdd)
                    && writes[2] == (DATA_PORT, 0x82)
                    && writes[3] == (DATA_PORT, Register::Duty as u8)
            })
            .count();
        // Each fan's duty changes twice. The third, stable tick adds no duty
        // transaction (fan-select writes do not count here).
        assert_eq!(duty_transactions, 4);

        let mut controller = Controller::new(EcMailbox::new(FakeIo::default()));
        controller
            .enable([curve(100), curve(100)], FanHysteresis::default(), 0)
            .unwrap();
        assert!(controller.direct_enabled());
        assert!(controller
            .sensor_failed("missing k10temp".into())
            .contains("released"));
        assert!(!controller.direct_enabled());
    }
}
