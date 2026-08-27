use z13helper_core::curve::Curve;
use z13helper_core::protocol::{FanHysteresis, ProbeReply};

use std::time::{Duration, Instant};

use crate::curve::{HysteresisState, TemperatureAverager, duty_at, hysteretic_temperature};
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
    duty_known: [bool; 2],
    last_ramp_at: Option<Instant>,
    consecutive_ec_errors: u32,
    release_failure: Option<String>,
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
            duty_known: [false; 2],
            last_ramp_at: None,
            consecutive_ec_errors: 0,
            release_failure: None,
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

    pub fn enable(
        &mut self,
        curves: [Curve; 2],
        hysteresis: FanHysteresis,
        temperature_average_seconds: u8,
    ) -> Result<(), String> {
        self.ec
            .set_global_mode(true)
            .map_err(|error| self.note_ec_error(error.to_string()))?;
        self.reset_runtime();
        self.consecutive_ec_errors = 0;
        self.curves = Some(curves);
        self.hysteresis = hysteresis;
        self.temperature_average_seconds = temperature_average_seconds;
        Ok(())
    }

    pub fn release(&mut self) -> Result<(), String> {
        self.reset_runtime();
        match self.ec.set_global_mode(false) {
            Ok(()) => {
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
            if self.duty_known[fan] && duty == self.last_duty[fan] {
                continue;
            }
            if let Err(error) = self.ec.set_duty(fan as u8, duty) {
                // A mailbox transaction may have reached the EC before
                // reporting an error. Do not treat the previous duty as
                // authoritative, and retry this fan on the next tick/prime.
                self.duty_known[fan] = false;
                return Err(self.note_ec_error(error.to_string()));
            }
            self.last_duty[fan] = duty;
            self.duty_known[fan] = true;
        }
        self.last_ramp_at = Some(now);
        self.consecutive_ec_errors = 0;
        Ok(())
    }

    pub fn sensor_failed(&mut self, error: String) -> String {
        match self.release_best_effort() {
            Some(release_error) => {
                format!("sensor failure; EC control release failed ({release_error}): {error}")
            }
            None => format!("sensor failure; EC control released: {error}"),
        }
    }

    pub fn direct_enabled(&self) -> bool {
        self.curves.is_some()
    }

    pub fn last_duty(&self) -> [u8; 2] {
        self.last_duty
    }

    pub fn take_release_failure(&mut self) -> Option<String> {
        self.release_failure.take()
    }

    fn note_ec_error(&mut self, error: String) -> String {
        self.consecutive_ec_errors = self.consecutive_ec_errors.saturating_add(1);
        if self.consecutive_ec_errors >= MAX_CONSECUTIVE_EC_ERRORS {
            match self.release_best_effort() {
                Some(release_error) => format!(
                    "repeated EC failure; control release failed ({release_error}): {error}"
                ),
                None => format!("repeated EC failure; control released: {error}"),
            }
        } else {
            format!("EC failure: {error}")
        }
    }

    fn release_best_effort(&mut self) -> Option<String> {
        self.reset_runtime();
        match self.ec.set_global_mode(false) {
            Ok(()) => None,
            Err(error) => {
                let error = error.to_string();
                self.release_failure = Some(error.clone());
                tracing::error!(%error, "failed to release EC automatic mode");
                Some(error)
            }
        }
    }

    fn reset_runtime(&mut self) {
        self.curves = None;
        self.temperature_average.clear();
        self.hysteresis_state = HysteresisState::default();
        self.last_duty = [0; 2];
        self.duty_known = [false; 2];
        self.last_ramp_at = None;
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
    use std::cell::{Cell, RefCell};
    use std::io;
    use std::rc::Rc;

    use super::*;
    use crate::ec::{COMMAND_STATUS_PORT, DATA_PORT, PortIo, Register};
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

    struct SwitchIo {
        fail_writes: Rc<Cell<bool>>,
        writes: Rc<RefCell<Vec<(u16, u8)>>>,
    }

    impl PortIo for SwitchIo {
        fn read_u8(&mut self, _port: u16) -> io::Result<u8> {
            Ok(0)
        }

        fn write_u8(&mut self, port: u16, value: u8) -> io::Result<()> {
            if self.fail_writes.get() {
                return Err(io::Error::other("write failed"));
            }
            self.writes.borrow_mut().push((port, value));
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

    fn duty_transaction_values(writes: &[(u16, u8)]) -> Vec<u8> {
        writes
            .windows(5)
            .filter(|writes| {
                writes[0] == (COMMAND_STATUS_PORT, 0xff)
                    && writes[1] == (COMMAND_STATUS_PORT, 0xdd)
                    && writes[2] == (DATA_PORT, 0x82)
                    && writes[3] == (DATA_PORT, Register::Duty as u8)
            })
            .map(|writes| writes[4].1)
            .collect()
    }

    #[test]
    fn ramp_reaches_targets_with_timing_and_bounds() {
        let mut duty = 0;
        for _ in 0..4 {
            duty = ramp_duty(duty, 200, DIRECT_TICK_INTERVAL);
            assert!(duty <= 200);
        }
        assert_eq!(duty, 200);

        duty = 255;
        for _ in 0..9 {
            duty = ramp_duty(duty, 20, DIRECT_TICK_INTERVAL);
            assert!(duty >= 20);
        }
        assert!(duty > 20);
        assert_eq!(ramp_duty(duty, 20, DIRECT_TICK_INTERVAL), 20);
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
    fn controller_prime_physically_writes_initial_zero_then_skips_unchanged_zero() {
        let mut controller = Controller::new(EcMailbox::new(FakeIo::default()));
        controller
            .enable([curve(0), curve(0)], FanHysteresis::default(), 0)
            .unwrap();
        let now = Instant::now();
        controller.prime(now, 60_000).unwrap();
        controller
            .prime(now + DIRECT_TICK_INTERVAL, 60_000)
            .unwrap();

        let io = controller.into_io();
        assert_eq!(duty_transaction_values(&io.writes), [0, 0]);
    }

    #[test]
    fn failed_zero_prime_is_unknown_and_retried_with_an_ec_write() {
        let fail_writes = Rc::new(Cell::new(false));
        let writes = Rc::new(RefCell::new(Vec::new()));
        let mut controller = Controller::new(EcMailbox::new(SwitchIo {
            fail_writes: fail_writes.clone(),
            writes: writes.clone(),
        }));
        controller
            .enable([curve(0), curve(0)], FanHysteresis::default(), 0)
            .unwrap();

        fail_writes.set(true);
        assert!(controller.prime(Instant::now(), 60_000).is_err());
        fail_writes.set(false);
        controller
            .prime(Instant::now() + DIRECT_TICK_INTERVAL, 60_000)
            .unwrap();

        assert_eq!(duty_transaction_values(&writes.borrow()), [0, 0]);
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
        assert!(
            controller
                .sensor_failed("missing k10temp".into())
                .contains("released")
        );
        assert!(!controller.direct_enabled());
    }

    #[test]
    fn failed_best_effort_release_is_reported() {
        let fail_writes = Rc::new(Cell::new(false));
        let mut controller = Controller::new(EcMailbox::new(SwitchIo {
            fail_writes: fail_writes.clone(),
            writes: Rc::new(RefCell::new(Vec::new())),
        }));
        controller
            .enable([curve(100), curve(100)], FanHysteresis::default(), 0)
            .unwrap();
        fail_writes.set(true);
        controller.sensor_failed("missing k10temp".into());
        assert!(controller.take_release_failure().is_some());
        fail_writes.set(false);
        assert!(controller.release().is_ok());
    }
}
