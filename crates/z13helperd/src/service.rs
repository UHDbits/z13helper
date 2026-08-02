use std::time::Instant;

use z13helper_core::curve::Curve;
use z13helper_core::protocol::{FanFloorConfig, ProbeReply};

use crate::curve::{controlled_duty, FloorGate};
use crate::ec::{EcMailbox, PortIo};
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
    floor: FanFloorConfig,
    gate: FloorGate,
    last_duty: [u8; 2],
    consecutive_ec_errors: u32,
    epoch: Instant,
}

impl<P: PortIo, S: SensorSource> Controller<P, S> {
    pub fn new(ec: EcMailbox<P>, sensors: S) -> Self {
        Self {
            ec,
            sensors,
            curves: None,
            floor: FanFloorConfig::default(),
            gate: FloorGate::default(),
            last_duty: [0; 2],
            consecutive_ec_errors: 0,
            epoch: Instant::now(),
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

    pub fn enable(&mut self, curves: [Curve; 2], floor: FanFloorConfig) -> Result<(), String> {
        self.ec
            .set_global_mode(true)
            .map_err(|error| self.note_ec_error(error.to_string()))?;
        self.curves = Some(curves);
        self.floor = floor;
        self.gate = FloorGate::default();
        self.consecutive_ec_errors = 0;
        self.tick()
    }

    pub fn release(&mut self) -> Result<(), String> {
        self.curves = None;
        self.gate = FloorGate::default();
        match self.ec.set_global_mode(false) {
            Ok(()) => {
                self.last_duty = [0; 2];
                self.consecutive_ec_errors = 0;
                Ok(())
            }
            Err(error) => Err(self.note_ec_error(error.to_string())),
        }
    }

    pub fn tick(&mut self) -> Result<(), String> {
        let Some(curves) = self.curves else {
            return Ok(());
        };
        let snapshot = match self.sensors.read() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.release_best_effort();
                return Err(format!("sensor failure; EC control released: {error}"));
            }
        };
        let now = self.epoch.elapsed().as_millis() as u64;
        let (first, gate) = controlled_duty(
            &curves[0],
            snapshot.apu_temperature_c,
            snapshot.pl1_w,
            self.floor,
            self.gate,
            now,
        );
        let (second, gate) = controlled_duty(
            &curves[1],
            snapshot.apu_temperature_c,
            snapshot.pl1_w,
            self.floor,
            gate,
            now,
        );
        self.gate = gate;
        let duties = [first, second];
        for (fan, duty) in duties.into_iter().enumerate() {
            if let Err(error) = self.ec.set_duty(fan as u8, duty) {
                return Err(self.note_ec_error(error.to_string()));
            }
        }
        self.last_duty = duties;
        self.consecutive_ec_errors = 0;
        Ok(())
    }

    pub fn direct_enabled(&self) -> bool {
        self.curves.is_some()
    }

    pub fn gate(&self) -> FloorGate {
        self.gate
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
        self.gate = FloorGate::default();
        if let Err(error) = self.ec.set_global_mode(false) {
            tracing::error!(%error, "failed to release EC automatic mode");
        }
    }
}
