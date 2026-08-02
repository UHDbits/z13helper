use std::collections::HashMap;

use z13helper_core::apply::{apply_request, Daemon};
use z13helper_core::curve::{Curve, TDP_MAX_SAFE};
use z13helper_core::error::DaemonError;
use z13helper_core::protocol::{
    ApplyRequest, ApplyResponse, Capabilities, DaemonState, FanFloorConfig, FloorEnforcement,
    FloorState, Health, LightingState, OverrideState, ProbeReply, TdpState, Telemetry,
    UndervoltState,
};

use crate::aura::AuraDevices;
use crate::curve::firmware_curve;
use crate::ec::{EcMailbox, LinuxPortIo};
use crate::sensors;
use crate::service::{Controller, HwmonSensors};
use crate::state::{PersistedState, StateStore};
use crate::sysfs::Sysfs;

pub struct PlatformHardware {
    sysfs: Sysfs,
    aura: AuraDevices,
    direct: Controller<LinuxPortIo, HwmonSensors>,
    floor: FanFloorConfig,
    undervolt_available: bool,
    ppd_profiles: Vec<String>,
    ppd_profile: Option<String>,
}

impl PlatformHardware {
    pub fn acquire() -> Result<(Self, ProbeReply), String> {
        let io = LinuxPortIo::acquire().map_err(|error| error.to_string())?;
        let mut direct = Controller::new(EcMailbox::new(io), HwmonSensors);
        let probe = direct.startup_release_and_probe()?;
        let sysfs = Sysfs::default();
        let undervolt_available = sysfs.probe_undervolt_once();
        let (ppd_profiles, ppd_profile) = Self::read_ppd().unwrap_or_default();
        let mut aura = AuraDevices::default();
        let _ = aura.refresh();
        Ok((
            Self {
                sysfs,
                aura,
                direct,
                floor: FanFloorConfig::default(),
                undervolt_available,
                ppd_profiles,
                ppd_profile,
            },
            probe,
        ))
    }

    pub fn set_floor(&mut self, floor: FanFloorConfig) {
        self.floor = floor;
    }

    pub fn tick(&mut self) -> Result<(), String> {
        self.direct.tick()
    }

    pub fn release_direct(&mut self) -> Result<(), String> {
        self.direct.release()
    }

    pub fn apply_lighting(&mut self, device: &str, state: &LightingState) -> Result<(), String> {
        self.aura.apply(device, state)
    }

    pub fn refresh_aura(&mut self) {
        let _ = self.aura.refresh();
    }

    pub fn refresh_new_aura(&mut self) -> Vec<String> {
        self.aura.refresh()
    }

    fn ppd_set_blocking(profile: &str) -> Result<(), String> {
        let connection = zbus::blocking::Connection::system().map_err(|error| error.to_string())?;
        let proxy = zbus::blocking::Proxy::new(
            &connection,
            "net.hadess.PowerProfiles",
            "/net/hadess/PowerProfiles",
            "net.hadess.PowerProfiles",
        )
        .map_err(|error| error.to_string())?;
        proxy
            .set_property("ActiveProfile", profile)
            .map_err(|error| error.to_string())
    }

    fn read_ppd() -> Result<(Vec<String>, Option<String>), String> {
        use zbus::zvariant::OwnedValue;

        let connection = zbus::blocking::Connection::system().map_err(|error| error.to_string())?;
        let proxy = zbus::blocking::Proxy::new(
            &connection,
            "net.hadess.PowerProfiles",
            "/net/hadess/PowerProfiles",
            "net.hadess.PowerProfiles",
        )
        .map_err(|error| error.to_string())?;
        let records: Vec<HashMap<String, OwnedValue>> = proxy
            .get_property("Profiles")
            .map_err(|error| error.to_string())?;
        let profiles = records
            .iter()
            .filter_map(|record| record.get("Profile"))
            .filter_map(|value| <&str>::try_from(value).ok())
            .map(str::to_owned)
            .collect();
        let current = proxy.get_property::<String>("ActiveProfile").ok();
        Ok((profiles, current))
    }

    fn refresh_ppd(&mut self) {
        if let Ok((profiles, current)) = Self::read_ppd() {
            self.ppd_profiles = profiles;
            self.ppd_profile = current;
        }
    }
}

impl Daemon for PlatformHardware {
    fn ppd_set(&mut self, profile: Option<&str>) -> Result<Option<String>, DaemonError> {
        let Some(profile) = profile else {
            return Ok(None);
        };
        if self.ppd_profiles.is_empty() {
            return Ok(Some(
                "power-profiles-daemon is unavailable; PPD selection was not applied".into(),
            ));
        }
        Self::ppd_set_blocking(profile).map_err(DaemonError::Rejected)?;
        self.ppd_profile = Some(profile.into());
        Ok(None)
    }

    fn tdp_set(&mut self, limits: TdpState, _force: bool) -> Result<(), DaemonError> {
        self.sysfs.set_tdp(limits).map_err(DaemonError::Rejected)
    }

    fn firmware_fans_set(
        &mut self,
        curves: &[Curve; 2],
        effective_pl1: u32,
    ) -> Result<(), DaemonError> {
        let written = [
            firmware_curve(&curves[0], effective_pl1, self.floor),
            firmware_curve(&curves[1], effective_pl1, self.floor),
        ];
        self.sysfs
            .set_firmware_curves(&written)
            .map_err(DaemonError::Rejected)?;
        self.direct.release().map_err(DaemonError::Rejected)
    }

    fn direct_fans_set(
        &mut self,
        curves: &[Curve; 2],
        _effective_pl1: u32,
    ) -> Result<(), DaemonError> {
        self.direct
            .enable(*curves, self.floor)
            .map_err(DaemonError::Rejected)
    }

    fn fans_release(&mut self) -> Result<(), DaemonError> {
        self.direct.release().map_err(DaemonError::Rejected)?;
        self.sysfs
            .release_firmware_fans()
            .map_err(DaemonError::Rejected)
    }

    fn undervolt_set(&mut self, cpu_co: i32) -> Result<(), DaemonError> {
        self.sysfs
            .set_undervolt(cpu_co)
            .map_err(DaemonError::Rejected)
    }

    fn undervolt_available(&self) -> bool {
        self.undervolt_available
    }
}

pub struct Backend {
    hardware: PlatformHardware,
    persisted: PersistedState,
    store: StateStore,
    probe: ProbeReply,
}

impl Backend {
    pub fn start() -> Result<Self, String> {
        let (hardware, probe) = PlatformHardware::acquire()?;
        let store = StateStore::default();
        let persisted = match store.load() {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(%error, "starting from fresh daemon state");
                let mut state = PersistedState::default();
                state.state.warnings.push(error);
                state
            }
        };
        let mut backend = Self {
            hardware,
            persisted,
            store,
            probe,
        };
        backend.observe();
        if let Some(desired) = backend.persisted.desired.clone() {
            if let Err(error) = backend.apply(desired) {
                backend.persisted.state.degraded = true;
                backend
                    .persisted
                    .state
                    .warnings
                    .push(format!("startup restore failed: {error}"));
            }
        } else {
            backend.persisted.state.fan_curves = Some(z13helper_core::stock_fan_curves(
                backend.persisted.state.ppd_profile.as_deref(),
            ));
            let _ = backend.save();
        }
        backend.restore_battery_policy();
        backend.restore_lighting();
        Ok(backend)
    }

    pub fn probe(&self) -> ProbeReply {
        self.probe.clone()
    }

    pub fn state(&mut self) -> DaemonState {
        self.observe();
        self.persisted.state.clone()
    }

    pub fn tick(&mut self) {
        if let Err(error) = self.hardware.tick() {
            tracing::warn!(%error, "direct fan tick failed");
            self.persisted.state.degraded = true;
            self.persisted.state.warnings.push(error);
        }
        self.observe();
    }

    pub fn apply(&mut self, request: ApplyRequest) -> Result<ApplyResponse, DaemonError> {
        self.prevalidate(&request)?;
        let previous = self.persisted.desired.clone();
        self.hardware.set_floor(request.floor);
        let warnings = match apply_request(&mut self.hardware, &request) {
            Ok(warnings) => warnings,
            Err(error) => {
                match previous {
                    Some(previous) => {
                        self.hardware.set_floor(previous.floor);
                        if let Err(rollback) = apply_request(&mut self.hardware, &previous) {
                            self.persisted.state.degraded = true;
                            self.persisted.state.warnings.push(format!(
                                "apply failed ({error}); rollback also failed ({rollback})"
                            ));
                        }
                    }
                    None => {
                        self.persisted.state.degraded = true;
                        self.persisted.state.warnings.push(format!(
                            "apply failed before any known-good daemon state existed: {error}"
                        ));
                    }
                }
                self.observe();
                let _ = self.save();
                return Err(error);
            }
        };
        self.persisted.desired = Some(request.clone());
        self.persisted.state.generation = self.persisted.state.generation.saturating_add(1);
        self.persisted.state.profile = Some(
            if request.power_limits.is_some()
                || request.fan_curves.is_some()
                || request.undervolt.is_some()
            {
                "custom"
            } else {
                request.ppd_profile.as_deref().unwrap_or("unmanaged")
            }
            .into(),
        );
        self.persisted.state.overrides = OverrideState {
            power: request.power_limits.is_some(),
            fans: request.fan_curves.is_some(),
            undervolt: request.undervolt.is_some(),
        };
        self.persisted.state.ppd_profile = request.ppd_profile.clone();
        self.persisted.state.tdp = request.power_limits;
        self.persisted.state.fan_curves =
            Some(request.fan_curves.unwrap_or_else(|| {
                z13helper_core::stock_fan_curves(request.ppd_profile.as_deref())
            }));
        self.persisted.state.fan_control_mode = request.fan_mode;
        self.persisted.state.floor_config = request.floor;
        self.persisted.state.undervolt = request.undervolt.map(|cpu_co| UndervoltState {
            cpu_co,
            active: self.hardware.undervolt_available,
        });
        self.persisted.state.warnings = warnings.clone();
        self.persisted.state.degraded = false;
        self.observe();
        self.save().map_err(DaemonError::Protocol)?;
        Ok(ApplyResponse {
            generation: self.persisted.state.generation,
            warnings,
        })
    }

    /// Apply a one-shot Curve Optimizer offset without changing the desired
    /// state that will be restored after suspend or daemon restart.
    pub fn apply_undervolt_once(&mut self, offset: i32) -> Result<(), DaemonError> {
        validate_manual_undervolt(offset, self.hardware.undervolt_available)?;
        self.hardware
            .sysfs
            .set_undervolt(offset)
            .map_err(DaemonError::Rejected)
    }

    pub fn factory_fan_curves(
        &mut self,
        ppd_profiles: Vec<String>,
    ) -> Result<HashMap<String, [Curve; 2]>, DaemonError> {
        if ppd_profiles.is_empty() {
            return Err(DaemonError::Rejected(
                "at least one PPD profile is required".into(),
            ));
        }
        self.hardware.refresh_ppd();
        for profile in &ppd_profiles {
            if !self
                .hardware
                .ppd_profiles
                .iter()
                .any(|known| known == profile)
            {
                return Err(DaemonError::Rejected(format!(
                    "unknown power-profiles-daemon profile {profile:?}"
                )));
            }
        }
        self.observe();
        validate_factory_curve_query_power(self.persisted.state.tdp)?;

        let previous = self.persisted.desired.clone();
        let original_ppd = self.hardware.ppd_profile.clone();
        if previous.is_none() && original_ppd.is_none() {
            return Err(DaemonError::Rejected(
                "current PPD profile is unknown; refusing a query that cannot be restored".into(),
            ));
        }

        let query = (|| {
            let mut curves = HashMap::new();
            for profile in ppd_profiles {
                PlatformHardware::ppd_set_blocking(&profile).map_err(DaemonError::Rejected)?;
                self.hardware.ppd_profile = Some(profile.clone());
                let factory = self
                    .hardware
                    .sysfs
                    .factory_fan_curves()
                    .map_err(DaemonError::Rejected)?;
                curves.insert(profile, factory);
            }
            Ok(curves)
        })();

        let restore = if let Some(previous) = previous {
            self.hardware.set_floor(previous.floor);
            apply_request(&mut self.hardware, &previous).map(|_| ())
        } else if let Some(original_ppd) = original_ppd {
            PlatformHardware::ppd_set_blocking(&original_ppd)
                .map_err(DaemonError::Rejected)
                .map(|_| {
                    self.hardware.ppd_profile = Some(original_ppd);
                })
        } else {
            unreachable!()
        };

        self.observe();
        if let Err(restore_error) = restore {
            let query_error = query
                .err()
                .map(|error| format!("query failed ({error}); "))
                .unwrap_or_default();
            let message =
                format!("{query_error}factory fan-curve restoration failed ({restore_error})");
            self.persisted.state.degraded = true;
            self.persisted.state.warnings.push(message.clone());
            let _ = self.save();
            return Err(DaemonError::Rejected(message));
        }
        query
    }

    pub fn set_battery_limit(&mut self, limit: i32) -> Result<(), DaemonError> {
        if !(40..=100).contains(&limit) {
            return Err(DaemonError::Rejected(
                "battery limit must be between 40 and 100".into(),
            ));
        }
        if !self.persisted.state.battery_one_time_charge {
            self.hardware
                .sysfs
                .set_battery_limit(limit)
                .map_err(DaemonError::Rejected)?;
        }
        self.persisted.state.battery_limit = Some(limit);
        self.bump_and_save()
    }

    pub fn set_battery_one_time_charge(&mut self, enabled: bool) -> Result<(), DaemonError> {
        if enabled == self.persisted.state.battery_one_time_charge {
            return Ok(());
        }
        if self.persisted.state.battery_limit.is_none() {
            self.persisted.state.battery_limit = Some(
                self.hardware
                    .sysfs
                    .battery_limit()
                    .map_err(DaemonError::Rejected)?,
            );
        }
        let target = effective_battery_limit(enabled, self.persisted.state.battery_limit)
            .expect("normal battery limit was initialized above");
        self.hardware
            .sysfs
            .set_battery_limit(target)
            .map_err(DaemonError::Rejected)?;
        self.persisted.state.battery_one_time_charge = enabled;
        self.bump_and_save()
    }

    pub fn set_panel_overdrive(&mut self, enabled: bool) -> Result<(), DaemonError> {
        self.hardware
            .sysfs
            .set_armoury_bool("panel_overdrive", enabled)
            .map_err(DaemonError::Rejected)?;
        self.persisted.state.panel_overdrive = Some(i32::from(enabled));
        self.bump_and_save()
    }

    pub fn set_lighting(
        &mut self,
        device: String,
        state: LightingState,
    ) -> Result<(), DaemonError> {
        self.hardware
            .apply_lighting(&device, &state)
            .map_err(DaemonError::Rejected)?;
        let devices = self
            .persisted
            .state
            .devices
            .get_or_insert_with(HashMap::new);
        devices.insert(device, state);
        self.bump_and_save()
    }

    pub fn release_fans(&mut self) -> Result<(), DaemonError> {
        if self
            .persisted
            .state
            .tdp
            .is_some_and(|tdp| tdp.pl1_spl > TDP_MAX_SAFE as i32)
        {
            return Err(DaemonError::Rejected(
                "lower PL1 to 75 W or below before releasing fan protection".into(),
            ));
        }
        self.hardware.fans_release()?;
        if let Some(desired) = self.persisted.desired.as_mut() {
            desired.fan_curves = None;
        }
        self.persisted.state.fan_curves = None;
        self.persisted.state.overrides.fans = false;
        self.persisted.state.floor = FloorState::default();
        self.bump_and_save()
    }

    pub fn shutdown(&mut self) {
        if let Err(error) = self.hardware.release_direct() {
            tracing::error!(%error, "failed to release EC control during shutdown");
        }
    }

    pub fn restore_volatile(&mut self) {
        if let Some(desired) = self.persisted.desired.clone() {
            if let Err(error) = self.apply(desired) {
                tracing::error!(%error, "failed to restore volatile hardware state");
                let _ = self.hardware.release_direct();
                return;
            }
        }
        self.restore_battery_policy();
        self.restore_lighting();
    }

    pub fn restore_hotplugged_lighting(&mut self) {
        let connected = self.hardware.refresh_new_aura();
        for device in connected {
            let state = self
                .persisted
                .state
                .devices
                .as_ref()
                .and_then(|devices| devices.get(&device))
                .cloned();
            if let Some(state) = state {
                if let Err(error) = self.hardware.apply_lighting(&device, &state) {
                    tracing::warn!(%error, %device, "failed to relight hotplugged device");
                }
            }
        }
    }

    fn observe(&mut self) {
        if let Ok(tdp) = self.hardware.sysfs.read_tdp() {
            self.persisted.state.tdp = Some(tdp);
        }
        if !self.persisted.state.battery_one_time_charge {
            if let Ok(limit) = self.hardware.sysfs.battery_limit() {
                self.persisted.state.battery_limit = Some(limit);
            }
        }
        if let Ok(battery) = self.hardware.sysfs.battery_telemetry() {
            self.persisted.state.battery = battery;
        }
        self.complete_one_time_charge_if_full();
        if let Ok(value) = self.hardware.sysfs.read_armoury_bool("panel_overdrive") {
            self.persisted.state.panel_overdrive = Some(value);
        }
        if let Ok(snapshot) = sensors::read_snapshot() {
            self.persisted.state.temperature = Some(snapshot.apu_temperature_c);
            self.persisted.state.fan_rpms = snapshot.rpm;
        }
        self.hardware.refresh_aura();
        self.hardware.refresh_ppd();
        self.persisted.state.undervolt_available = self.hardware.undervolt_available;
        self.persisted.state.ppd_profile = self.hardware.ppd_profile.clone();
        self.persisted.state.capabilities = Capabilities {
            ppd_available: !self.hardware.ppd_profiles.is_empty(),
            ppd_profiles: self.hardware.ppd_profiles.clone(),
            firmware_fans: true,
            direct_fans: true,
            undervolt: self.hardware.undervolt_available,
            keyboard_lighting: self.hardware.aura.available("keyboard"),
            lightbar_lighting: self.hardware.aura.available("lightbar"),
        };
        let pl1 = self
            .persisted
            .state
            .tdp
            .map(|state| state.pl1_spl.max(0) as u32)
            .unwrap_or(0);
        let direct = self.hardware.direct.direct_enabled();
        let gate = self.hardware.direct.gate();
        self.persisted.state.floor = if direct {
            FloorState {
                enforcement: if gate.engaged {
                    FloorEnforcement::DirectEngaged
                } else {
                    FloorEnforcement::DirectReleased
                },
                armed: pl1 > TDP_MAX_SAFE,
                engaged: gate.engaged,
                effective_min_duty: if gate.engaged {
                    self.persisted.state.floor_config.duty
                } else {
                    0
                },
            }
        } else if pl1 > TDP_MAX_SAFE && self.persisted.state.fan_curves.is_some() {
            FloorState {
                enforcement: FloorEnforcement::FirmwareArmed,
                armed: true,
                engaged: self.persisted.state.temperature.is_some_and(|temperature| {
                    temperature >= self.persisted.state.floor_config.engage_temp_c
                }),
                effective_min_duty: self.persisted.state.floor_config.duty,
            }
        } else {
            FloorState::default()
        };
        self.persisted.state.telemetry = Telemetry {
            temperature_c: self.persisted.state.temperature,
            fan_rpms: self.persisted.state.fan_rpms,
        };
        self.persisted.state.health = Health {
            degraded: self.persisted.state.degraded,
            warnings: self.persisted.state.warnings.clone(),
        };
    }

    fn bump_and_save(&mut self) -> Result<(), DaemonError> {
        self.persisted.state.generation = self.persisted.state.generation.saturating_add(1);
        self.save().map_err(DaemonError::Protocol)
    }

    fn save(&self) -> Result<(), String> {
        self.store.save(&self.persisted)
    }

    fn restore_battery_policy(&mut self) {
        let target = effective_battery_limit(
            self.persisted.state.battery_one_time_charge,
            self.persisted.state.battery_limit,
        );
        if let Some(target) = target {
            if let Err(error) = self.hardware.sysfs.set_battery_limit(target) {
                tracing::warn!(%error, target, "failed to restore battery charge policy");
            }
        }
    }

    fn complete_one_time_charge_if_full(&mut self) {
        if !one_time_charge_is_complete(
            self.persisted.state.battery_one_time_charge,
            self.persisted.state.battery.charge_percent,
        ) {
            return;
        }
        let Some(limit) = self.persisted.state.battery_limit else {
            return;
        };
        match self.hardware.sysfs.set_battery_limit(limit) {
            Ok(()) => {
                let previous_generation = self.persisted.state.generation;
                self.persisted.state.battery_one_time_charge = false;
                if let Err(error) = self.bump_and_save() {
                    self.persisted.state.battery_one_time_charge = true;
                    self.persisted.state.generation = previous_generation;
                    tracing::error!(%error, "failed to persist completed one-time charge");
                }
            }
            Err(error) => {
                tracing::warn!(%error, limit, "failed to restore battery limit after full charge");
            }
        }
    }

    fn restore_lighting(&mut self) {
        let lighting = self.persisted.state.devices.clone().unwrap_or_default();
        for (device, state) in lighting {
            if let Err(error) = self.hardware.apply_lighting(&device, &state) {
                tracing::warn!(%error, %device, "failed to restore lighting");
            }
        }
    }

    fn prevalidate(&self, request: &ApplyRequest) -> Result<(), DaemonError> {
        request.validate().map_err(DaemonError::Rejected)?;
        if let Some(profile) = request.ppd_profile.as_deref() {
            if !self.hardware.ppd_profiles.is_empty()
                && !self
                    .hardware
                    .ppd_profiles
                    .iter()
                    .any(|known| known == profile)
            {
                return Err(DaemonError::Rejected(format!(
                    "unknown power-profiles-daemon profile {profile:?}"
                )));
            }
        }
        Ok(())
    }
}

fn effective_battery_limit(one_time_charge: bool, normal_limit: Option<i32>) -> Option<i32> {
    one_time_charge.then_some(100).or(normal_limit)
}

fn validate_factory_curve_query_power(tdp: Option<TdpState>) -> Result<(), DaemonError> {
    let pl1 = tdp.map(|tdp| tdp.pl1_spl.max(0) as u32).unwrap_or(0);
    if pl1 > TDP_MAX_SAFE {
        return Err(DaemonError::Rejected(format!(
            "factory fan curves cannot be read while PL1 is {pl1} W; lower power first"
        )));
    }
    Ok(())
}

fn one_time_charge_is_complete(one_time_charge: bool, charge_percent: Option<u8>) -> bool {
    one_time_charge && charge_percent.is_some_and(|charge| charge >= 100)
}

fn validate_manual_undervolt(offset: i32, available: bool) -> Result<(), DaemonError> {
    if !(-40..=0).contains(&offset) {
        return Err(DaemonError::Rejected(
            "Curve Optimizer offset must be between -40 and 0".into(),
        ));
    }
    if !available {
        return Err(DaemonError::Rejected(
            "ryzen_smu is unavailable; undervolt cannot be applied".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        effective_battery_limit, one_time_charge_is_complete, validate_factory_curve_query_power,
        validate_manual_undervolt,
    };
    use z13helper_core::protocol::TdpState;

    #[test]
    fn one_time_charge_overrides_and_then_restores_the_normal_limit() {
        assert_eq!(effective_battery_limit(false, Some(80)), Some(80));
        assert_eq!(effective_battery_limit(true, Some(80)), Some(100));
        assert!(!one_time_charge_is_complete(true, Some(99)));
        assert!(one_time_charge_is_complete(true, Some(100)));
        assert!(!one_time_charge_is_complete(false, Some(100)));
    }

    #[test]
    fn factory_curve_query_never_drops_high_power_protection() {
        let tdp = |pl1| TdpState {
            pl1_spl: pl1,
            ..TdpState::default()
        };
        assert!(validate_factory_curve_query_power(Some(tdp(75))).is_ok());
        assert!(validate_factory_curve_query_power(Some(tdp(76))).is_err());
    }

    #[test]
    fn one_shot_undervolt_requires_a_safe_available_offset() {
        assert!(validate_manual_undervolt(-20, true).is_ok());
        assert!(validate_manual_undervolt(-41, true).is_err());
        assert!(validate_manual_undervolt(-20, false).is_err());
    }
}
