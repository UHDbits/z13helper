use crate::curve::{controlled_duty, Curve};
use crate::ec::{EcMailbox, PortIo};
use crate::protocol::{FanBroker, ProbeReply, StatusReply};
use crate::sensors::{self, SensorSnapshot};

const MODEL: &str = "GZ302EA";
const MAX_CONSECUTIVE_EC_ERRORS: u32 = 3;

pub trait SensorSource {
    fn read(&mut self) -> Result<SensorSnapshot, String>;
}

pub struct HwmonSensors;

impl SensorSource for HwmonSensors {
    fn read(&mut self) -> Result<SensorSnapshot, String> {
        sensors::read_snapshot().map_err(|error| error.to_string())
    }
}

pub struct Controller<P, S> {
    ec: EcMailbox<P>,
    sensors: S,
    curves: Option<[Curve; 2]>,
    last_duty: [u8; 2],
    consecutive_ec_errors: u32,
}

impl<P: PortIo, S: SensorSource> Controller<P, S> {
    pub fn new(ec: EcMailbox<P>, sensors: S) -> Self {
        Self {
            ec,
            sensors,
            curves: None,
            last_duty: [0; 2],
            consecutive_ec_errors: 0,
        }
    }

    pub fn startup_release_and_probe(&mut self) -> Result<ProbeReply, String> {
        self.release()?;
        self.probe()
    }

    pub fn tick(&mut self) -> Result<(), String> {
        let Some(curves) = self.curves.clone() else {
            return Ok(());
        };
        let snapshot = match self.sensors.read() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.release_best_effort();
                return Err(format!("sensor failure; EC control released: {error}"));
            }
        };
        let duties = [
            controlled_duty(&curves[0], snapshot.apu_temperature_c, snapshot.pl1_w),
            controlled_duty(&curves[1], snapshot.apu_temperature_c, snapshot.pl1_w),
        ];

        for (fan, duty) in duties.into_iter().enumerate() {
            if let Err(error) = self.ec.set_duty(fan as u8, duty) {
                return Err(self.note_ec_error(error.to_string()));
            }
        }
        self.last_duty = duties;
        self.consecutive_ec_errors = 0;
        Ok(())
    }

    pub fn shutdown_release(&mut self) -> Result<(), String> {
        self.release()
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
        if let Err(error) = self.ec.set_global_mode(false) {
            eprintln!("failed to release EC automatic mode: {error}");
        }
    }
}

impl<P: PortIo, S: SensorSource> FanBroker for Controller<P, S> {
    fn probe(&mut self) -> Result<ProbeReply, String> {
        match self.ec.probe() {
            Ok(probe) => {
                self.consecutive_ec_errors = 0;
                Ok(ProbeReply {
                    model: MODEL.to_owned(),
                    ec_version: probe.version,
                    fan_count: probe.fan_count,
                })
            }
            Err(error) => Err(self.note_ec_error(error.to_string())),
        }
    }

    fn status(&mut self) -> Result<StatusReply, String> {
        let snapshot = match self.sensors.read() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.release_best_effort();
                return Err(format!("sensor failure; EC control released: {error}"));
            }
        };
        Ok(StatusReply {
            enabled: self.curves.is_some(),
            apu_temperature_c: snapshot.apu_temperature_c,
            rpm: snapshot.rpm,
            pl1_w: snapshot.pl1_w,
            duty: self.last_duty,
            ec_errors: self.consecutive_ec_errors,
        })
    }

    fn enable(&mut self, curves: [Curve; 2]) -> Result<(), String> {
        if let Err(error) = self.ec.set_global_mode(true) {
            return Err(self.note_ec_error(error.to_string()));
        }
        self.curves = Some(curves);
        self.consecutive_ec_errors = 0;
        // Apply the first duty immediately so fans do not sit frozen at the
        // previous firmware duty until the next 1 Hz tick.
        self.tick()
    }

    fn release(&mut self) -> Result<(), String> {
        self.curves = None;
        match self.ec.set_global_mode(false) {
            Ok(()) => {
                self.last_duty = [0; 2];
                self.consecutive_ec_errors = 0;
                Ok(())
            }
            Err(error) => Err(self.note_ec_error(error.to_string())),
        }
    }
}
