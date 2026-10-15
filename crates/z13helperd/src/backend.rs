use std::collections::{HashMap, HashSet};

use z13helper_core::apply::{Daemon, apply_request, set_fans};
use z13helper_core::curve::{
    Curve, HIGH_POWER_THRESHOLD_W, fan_protection_required, high_power_curve,
};
use z13helper_core::error::DaemonError;
use z13helper_core::profile::FanControlMode;
use z13helper_core::protocol::{
    ApplyRequest, ApplyResponse, Capabilities, DaemonState, FanHysteresis, Health, LightingState,
    OverrideState, ProbeReply, TdpState, Telemetry, UndervoltState, stock_tdp, validate_undervolt,
};

use crate::aura::AuraDevices;
use crate::direct_runtime::{DirectRuntime, DirectSnapshot};
use crate::sensors;
use crate::state::{PersistedState, StateLoadError, StateStore};
use crate::sysfs::Sysfs;

const MAX_WARNINGS: usize = 32;
const MAX_WARNING_CHARS: usize = 1024;
const MAX_FACTORY_CURVE_PROFILES: usize = 8;
const BATTERY_LIMIT_RANGE: std::ops::RangeInclusive<i32> = 40..=100;

fn push_warning(warnings: &mut Vec<String>, warning: impl Into<String>) {
    let warning = warning
        .into()
        .chars()
        .take(MAX_WARNING_CHARS)
        .collect::<String>();
    if warning.is_empty() || warnings.iter().any(|existing| existing == &warning) {
        return;
    }
    if warnings.len() == MAX_WARNINGS {
        warnings.remove(0);
    }
    warnings.push(warning);
}

pub struct PlatformHardware {
    sysfs: Sysfs,
    aura: AuraDevices,
    direct: DirectRuntime,
    direct_snapshot: DirectSnapshot,
    latest_temperature_millic: Option<i32>,
    fan_hysteresis: FanHysteresis,
    fan_temperature_average_seconds: u8,
    disable_high_power_fan_protection: bool,
    undervolt_available: bool,
    ppd_profiles: Vec<String>,
    ppd_profile: Option<String>,
    direct_release_failure: Option<String>,
}

impl PlatformHardware {
    fn fan_curves_for_hardware(&self, curves: &[Curve; 2], effective_pl1: u32) -> [Curve; 2] {
        if fan_protection_required(effective_pl1, self.disable_high_power_fan_protection) {
            [high_power_curve(&curves[0]), high_power_curve(&curves[1])]
        } else {
            *curves
        }
    }

    pub fn acquire() -> Result<(Self, ProbeReply), String> {
        let (direct, probe) = DirectRuntime::start()?;
        let sysfs = Sysfs::default();
        let undervolt_available = sysfs.probe_undervolt_once();
        let (ppd_profiles, ppd_profile) = Self::read_ppd().unwrap_or_default();
        let mut aura = AuraDevices::default();
        // Startup restore_lighting lights the devices present now.
        let _ = aura.take_reconnected();
        Ok((
            Self {
                sysfs,
                aura,
                direct,
                direct_snapshot: DirectSnapshot::default(),
                latest_temperature_millic: None,
                fan_hysteresis: FanHysteresis::default(),
                fan_temperature_average_seconds:
                    z13helper_core::profile::default_fan_temperature_average_seconds(),
                disable_high_power_fan_protection: false,
                undervolt_available,
                ppd_profiles,
                ppd_profile,
                direct_release_failure: None,
            },
            probe,
        ))
    }

    pub fn set_fan_policy(
        &mut self,
        hysteresis: FanHysteresis,
        temperature_average_seconds: u8,
        disable_high_power: bool,
    ) -> Result<(), String> {
        // Fields first: Backend restores the committed policy through this
        // call and must get the override flag back even if the owner fails.
        self.fan_hysteresis = hysteresis;
        self.fan_temperature_average_seconds = temperature_average_seconds;
        self.disable_high_power_fan_protection = disable_high_power;
        self.direct
            .set_policy(hysteresis, temperature_average_seconds)
    }

    fn observed_temperature_c(&mut self) -> Option<i32> {
        if !self.direct_snapshot.enabled || self.latest_temperature_millic.is_none() {
            self.latest_temperature_millic = sensors::read_temperature_millic().ok();
        }
        self.latest_temperature_millic.map(|temperature| {
            if temperature >= 0 {
                (temperature + 500) / 1_000
            } else {
                (temperature - 500) / 1_000
            }
        })
    }

    pub fn release_direct(&mut self) -> Result<(), String> {
        match self.direct.release() {
            Ok(snapshot) => {
                self.sync_direct_snapshot(snapshot);
                Ok(())
            }
            Err(error) => {
                self.direct_release_failure = Some(error.clone());
                Err(error)
            }
        }
    }

    fn sync_direct_snapshot(&mut self, snapshot: DirectSnapshot) {
        self.latest_temperature_millic = snapshot.temperature_millic;
        if let Some(error) = snapshot.release_failure.clone() {
            self.direct_release_failure = Some(error);
        }
        self.direct_snapshot = snapshot;
    }

    fn ppd_proxy() -> Result<zbus::blocking::Proxy<'static>, String> {
        const NAME: &str = "net.hadess.PowerProfiles";
        const PATH: &str = "/net/hadess/PowerProfiles";
        let connection = zbus::blocking::Connection::system().map_err(|error| error.to_string())?;
        let running = zbus::blocking::fdo::DBusProxy::new(&connection)
            .and_then(|dbus| Ok(dbus.name_has_owner(NAME.try_into()?)?))
            .map_err(|error| error.to_string())?;
        if !running {
            // PPD's stock unit is After=multi-user.target, so blocking on its
            // D-Bus activation during startup deadlocks the apply-settings unit
            // that multi-user.target waits for. Activate it without waiting;
            // the next observe tick reads it once it is up.
            let _ =
                zbus::blocking::Proxy::new(&connection, NAME, PATH, "org.freedesktop.DBus.Peer")
                    .and_then(|peer| peer.call_noreply("Ping", &()));
            return Err("power-profiles-daemon is not running yet".into());
        }
        zbus::blocking::Proxy::new(&connection, NAME, PATH, NAME).map_err(|error| error.to_string())
    }

    fn ppd_set_blocking(profile: &str) -> Result<(), String> {
        Self::ppd_proxy()?
            .set_property("ActiveProfile", profile)
            .map_err(|error| error.to_string())
    }

    fn read_ppd() -> Result<(Vec<String>, Option<String>), String> {
        use zbus::zvariant::OwnedValue;

        let proxy = Self::ppd_proxy()?;
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
        match Self::read_ppd() {
            Ok((profiles, current)) => {
                self.ppd_profiles = profiles;
                self.ppd_profile = current;
            }
            Err(error) => {
                tracing::debug!(%error, "power-profiles-daemon is unavailable");
                self.ppd_profiles.clear();
                self.ppd_profile = None;
            }
        }
    }

    fn check_fan_release_power(&self) -> Result<(), DaemonError> {
        let actual = self.sysfs.read_tdp().map_err(|error| {
            DaemonError::Rejected(format!(
                "cannot prove current PL1 before releasing fan protection: {error}"
            ))
        })?;
        validate_fan_release_power(actual, self.disable_high_power_fan_protection)
    }
}

trait ShutdownHardware {
    fn release_direct_for_shutdown(&mut self) -> Result<(), String>;
    fn on_battery_for_shutdown(&self) -> Result<bool, String>;
}

impl ShutdownHardware for PlatformHardware {
    fn release_direct_for_shutdown(&mut self) -> Result<(), String> {
        self.release_direct()
    }

    fn on_battery_for_shutdown(&self) -> Result<bool, String> {
        self.sysfs.on_battery()
    }
}

fn shutdown_hardware_actions(
    hardware: &mut impl ShutdownHardware,
) -> (Result<(), String>, Result<bool, String>) {
    let release = hardware.release_direct_for_shutdown();
    let power_source = hardware.on_battery_for_shutdown();
    (release, power_source)
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

    fn tdp_set(&mut self, limits: TdpState) -> Result<Option<String>, DaemonError> {
        self.sysfs.set_tdp(limits).map_err(DaemonError::Rejected)
    }

    fn firmware_fans_set(
        &mut self,
        curves: &[Curve; 2],
        effective_pl1: u32,
    ) -> Result<(), DaemonError> {
        let written = self.fan_curves_for_hardware(curves, effective_pl1);
        let sysfs = &self.sysfs;
        let result = firmware_transition(&mut self.direct, || sysfs.set_firmware_curves(&written));
        if let Ok(snapshot) = self.direct.snapshot() {
            self.sync_direct_snapshot(snapshot);
        }
        result.map_err(DaemonError::Rejected)
    }

    fn direct_fans_set(
        &mut self,
        curves: &[Curve; 2],
        effective_pl1: u32,
    ) -> Result<(), DaemonError> {
        let written = self.fan_curves_for_hardware(curves, effective_pl1);
        match self.direct.install_and_prime(written) {
            Ok(snapshot) => {
                self.sync_direct_snapshot(snapshot);
                Ok(())
            }
            Err(error) => Err(DaemonError::Rejected(match self.release_direct() {
                Err(release) => format!("{error}; direct EC release failed: {release}"),
                Ok(()) => error,
            })),
        }
    }

    fn current_pl1(&self) -> Result<u32, DaemonError> {
        self.sysfs
            .read_tdp()
            .map(TdpState::pl1_watts)
            .map_err(|error| {
                DaemonError::Rejected(format!(
                    "cannot prove current PL1 before writing fan curves: {error}"
                ))
            })
    }

    fn fans_release(&mut self) -> Result<(), DaemonError> {
        self.check_fan_release_power()?;
        self.release_direct().map_err(DaemonError::Rejected)?;
        self.sysfs
            .release_firmware_fans()
            .map_err(DaemonError::Rejected)
    }

    fn undervolt_set(&mut self, cpu_co: i32) -> Result<(), DaemonError> {
        self.sysfs
            .set_undervolt(cpu_co)
            .map_err(DaemonError::Rejected)
    }

    fn cpu_temp_limit_set(&mut self, temperature_c: u8) -> Result<(), DaemonError> {
        self.sysfs
            .set_cpu_temp_limit(temperature_c)
            .map_err(DaemonError::Rejected)
    }

    fn undervolt_available(&self) -> bool {
        self.undervolt_available
    }
}

/// Serialize the firmware handoff with the direct owner. A successful writer
/// call is the confirmation that both firmware curves and their enable state
/// were accepted; only then may EC automatic mode be restored. On failure the
/// direct owner resumes from its retained safe duty instead of being released.
fn firmware_transition(
    direct: &mut DirectRuntime,
    write: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    direct.hold_for_firmware()?;
    match write() {
        Ok(()) => direct.release().map(|_| ()),
        Err(error) => match direct.resume_after_firmware_failure() {
            Ok(_) => Err(error),
            Err(resume) => Err(format!(
                "{error}; direct EC resume after firmware write failure failed: {resume}"
            )),
        },
    }
}

pub struct Backend {
    hardware: PlatformHardware,
    persisted: PersistedState,
    store: Box<dyn StateWriter>,
    persistence: PersistenceMode,
    probe: ProbeReply,
    suspended_on_battery: Option<bool>,
}

trait StateWriter {
    fn save(&self, state: &PersistedState) -> Result<(), String>;
}

impl StateWriter for StateStore {
    fn save(&self, state: &PersistedState) -> Result<(), String> {
        StateStore::save(self, state)
    }
}

#[derive(Clone, Copy)]
struct FanProtectionSnapshot {
    curves: [Curve; 2],
    hysteresis: FanHysteresis,
    temperature_average_seconds: u8,
    disable_high_power_fan_protection: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistenceMode {
    ReadWrite,
    UnsupportedReadOnly { version: u32 },
    UnreadableReadOnly,
}

#[derive(Clone, Debug)]
enum FactoryQueryRestore {
    Desired(ApplyRequest),
    PpdStock { profile: String, tdp: TdpState },
}

impl PersistenceMode {
    fn require_writable(self) -> Result<(), DaemonError> {
        match self {
            Self::ReadWrite => Ok(()),
            Self::UnsupportedReadOnly { version } => Err(DaemonError::Rejected(format!(
                "daemon state version {version} is unsupported; persistent controls are read-only"
            ))),
            Self::UnreadableReadOnly => Err(DaemonError::Rejected(
                "daemon state could not be read or preserved; persistent controls are read-only"
                    .into(),
            )),
        }
    }
}

impl Backend {
    pub fn start() -> Result<Self, String> {
        let (hardware, probe) = PlatformHardware::acquire()?;
        let store = StateStore::default();
        let (persisted, persistence) = loaded_state(store.load());
        let mut backend = Self {
            hardware,
            persisted,
            store: Box::new(store),
            persistence,
            probe,
            suspended_on_battery: None,
        };
        backend.restore_panel_overdrive();
        backend.observe();
        if backend.persisted.desired.is_some() {
            backend.restore_desired("startup restore");
        } else {
            backend.persisted.state.fan_curves = Some(z13helper_core::stock_fan_curves(
                backend.persisted.state.ppd_profile.as_deref(),
            ));
            if persistence == PersistenceMode::ReadWrite
                && let Err(error) = backend.save()
            {
                tracing::warn!(%error, "failed to persist initial daemon state");
            }
        }
        backend.restore_battery_policy();
        backend.restore_lighting();
        Ok(backend)
    }

    pub fn probe(&self) -> ProbeReply {
        self.probe.clone()
    }

    /// Return the latest one-second observation snapshot. Socket reads must not
    /// trigger D-Bus, sysfs, HID, or SMU work on the request path.
    pub fn state(&self) -> DaemonState {
        self.persisted.state.clone()
    }

    pub fn apply(&mut self, request: ApplyRequest) -> Result<ApplyResponse, DaemonError> {
        self.persistence.require_writable()?;
        self.hardware.refresh_ppd();
        self.prevalidate(&request)?;
        let previous_persisted = self.persisted.clone();
        let previous = self.persisted.desired.clone();
        if let Err(error) = self.hardware.set_fan_policy(
            request.fan_hysteresis,
            request.fan_temperature_average_seconds,
            request.disable_high_power_fan_protection,
        ) {
            self.restore_committed_fan_policy();
            return Err(DaemonError::Rejected(error));
        }
        let warnings = match apply_request(&mut self.hardware, &request) {
            Ok(warnings) => warnings,
            Err(error) => {
                self.record_direct_release_failure("apply");
                self.restore_committed_fan_policy();
                match previous {
                    Some(previous) => {
                        if let Err(rollback) = apply_request(&mut self.hardware, &previous) {
                            self.persisted.state.degraded = true;
                            push_warning(
                                &mut self.persisted.state.warnings,
                                format!(
                                    "apply failed ({error}); rollback also failed ({rollback})"
                                ),
                            );
                        }
                    }
                    None => {
                        self.persisted.state.degraded = true;
                        push_warning(
                            &mut self.persisted.state.warnings,
                            format!(
                                "apply failed before any known-good daemon state existed: {error}"
                            ),
                        );
                    }
                }
                self.record_direct_release_failure("apply");
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
                || request.cpu_temp_limit != z13helper_core::profile::default_cpu_temp_limit()
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
        self.persisted.state.cpu_temp_limit = Some(request.cpu_temp_limit);
        self.persisted.state.fan_hysteresis = request.fan_hysteresis;
        self.persisted.state.fan_temperature_average_seconds =
            request.fan_temperature_average_seconds;
        self.persisted.state.disable_high_power_fan_protection =
            request.disable_high_power_fan_protection;
        self.persisted.state.undervolt = request.undervolt.map(|cpu_co| UndervoltState {
            cpu_co,
            active: self.hardware.undervolt_available,
        });
        self.persisted.state.warnings = warnings.clone();
        self.persisted.state.degraded = false;
        self.record_direct_release_failure("apply");
        self.observe();
        if let Err(error) = self.save() {
            self.persisted = previous_persisted;
            self.restore_committed_fan_policy();
            let rollback = match previous {
                Some(previous) => apply_request(&mut self.hardware, &previous).map(|_| ()),
                None => Err(DaemonError::Rejected(
                    "no known-good hardware state; retained current fan protection".into(),
                )),
            };
            self.persisted.state.degraded = true;
            push_warning(
                &mut self.persisted.state.warnings,
                match rollback {
                    Ok(()) => format!("apply persistence failed ({error}); hardware rolled back"),
                    Err(rollback) => format!(
                        "apply persistence failed ({error}); rollback also failed ({rollback})"
                    ),
                },
            );
            if let Err(repair) = self.save() {
                push_warning(
                    &mut self.persisted.state.warnings,
                    format!("apply persistence rollback could not be saved: {repair}"),
                );
                let _ = self.save();
            }
            self.record_direct_release_failure("apply persistence rollback");
            self.observe();
            return Err(DaemonError::Protocol(error));
        }
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
        let ppd_profiles = normalize_factory_curve_profiles(ppd_profiles)?;
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
        let actual_tdp = self.hardware.sysfs.read_tdp().map_err(|error| {
            DaemonError::Rejected(format!(
                "cannot prove current PL1 before reading factory fan curves: {error}"
            ))
        })?;
        validate_factory_curve_query_power(actual_tdp)?;

        let previous = self.persisted.desired.clone();
        let original_ppd = self.hardware.ppd_profile.clone();
        let restore_plan = factory_query_restore_plan(previous, original_ppd)?;

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

        let restore = match restore_plan {
            FactoryQueryRestore::Desired(previous) => self
                .hardware
                .set_fan_policy(
                    previous.fan_hysteresis,
                    previous.fan_temperature_average_seconds,
                    previous.disable_high_power_fan_protection,
                )
                .map_err(DaemonError::Rejected)
                .and_then(|_| apply_request(&mut self.hardware, &previous).map(|_| ())),
            FactoryQueryRestore::PpdStock {
                profile: original_ppd,
                tdp,
            } => {
                // With no daemon-owned desired PPT table, selecting the
                // original PPD profile is the only restoration contract
                // established by this repository. It intentionally
                // normalizes to PPD's stock table; preserving arbitrary
                // externally-owned five-node PPT values remains unproven.
                PlatformHardware::ppd_set_blocking(&original_ppd)
                    .and_then(|_| self.hardware.sysfs.set_tdp(tdp))
                    .map_err(DaemonError::Rejected)
                    // Stock fast PPT (<= 86 W) sits at or below the observed firmware cap.
                    .map(|_| {
                        self.hardware.ppd_profile = Some(original_ppd);
                    })
            }
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
            push_warning(&mut self.persisted.state.warnings, message.clone());
            let _ = self.save();
            return Err(DaemonError::Rejected(message));
        }
        query
    }

    pub fn set_battery_limit(&mut self, limit: i32) -> Result<(), DaemonError> {
        self.persistence.require_writable()?;
        if !BATTERY_LIMIT_RANGE.contains(&limit) {
            return Err(DaemonError::Rejected(
                "battery limit must be between 40 and 100".into(),
            ));
        }
        let previous = self.persisted.clone();
        let mut candidate = previous.clone();
        let previous_hardware_limit = self
            .hardware
            .sysfs
            .battery_limit()
            .map_err(DaemonError::Rejected)?;
        let target = effective_battery_limit(previous.state.battery_one_time_charge, Some(limit))
            .unwrap_or(limit);
        candidate.state.battery_limit = Some(limit);
        self.apply_and_commit_auxiliary(
            previous,
            candidate,
            "battery limit",
            move |backend| backend.hardware.sysfs.set_battery_limit(target),
            move |backend| {
                backend
                    .hardware
                    .sysfs
                    .set_battery_limit(previous_hardware_limit)
            },
        )
    }

    pub fn set_battery_one_time_charge(&mut self, enabled: bool) -> Result<(), DaemonError> {
        self.persistence.require_writable()?;
        if enabled == self.persisted.state.battery_one_time_charge {
            return Ok(());
        }
        let previous = self.persisted.clone();
        let mut candidate = previous.clone();
        let previous_hardware_limit = self
            .hardware
            .sysfs
            .battery_limit()
            .map_err(DaemonError::Rejected)?;
        let normal_limit = *candidate
            .state
            .battery_limit
            .get_or_insert(previous_hardware_limit);
        let target = effective_battery_limit(enabled, Some(normal_limit)).unwrap_or(normal_limit);
        candidate.state.battery_one_time_charge = enabled;
        self.apply_and_commit_auxiliary(
            previous,
            candidate,
            "one-time battery charge",
            move |backend| backend.hardware.sysfs.set_battery_limit(target),
            move |backend| {
                backend
                    .hardware
                    .sysfs
                    .set_battery_limit(previous_hardware_limit)
            },
        )
    }

    pub fn set_panel_overdrive(&mut self, enabled: bool) -> Result<(), DaemonError> {
        self.persistence.require_writable()?;
        let previous = self.persisted.clone();
        let mut candidate = previous.clone();
        let previous_hardware_value = self
            .hardware
            .sysfs
            .read_panel_overdrive()
            .map_err(DaemonError::Rejected)?;
        candidate.state.panel_overdrive = Some(i32::from(enabled));
        self.apply_and_commit_auxiliary(
            previous,
            candidate,
            "panel overdrive",
            move |backend| backend.hardware.sysfs.set_panel_overdrive(enabled),
            move |backend| {
                backend
                    .hardware
                    .sysfs
                    .set_panel_overdrive(previous_hardware_value != 0)
            },
        )
    }

    pub fn set_lighting(
        &mut self,
        device: String,
        state: LightingState,
    ) -> Result<(), DaemonError> {
        self.persistence.require_writable()?;
        let previous = self.persisted.clone();
        let previous_device_state = previous
            .state
            .devices
            .as_ref()
            .and_then(|devices| devices.get(&device))
            .cloned();
        let mut candidate = previous.clone();
        let apply_device = device.clone();
        let apply_state = state.clone();
        candidate
            .state
            .devices
            .get_or_insert_with(HashMap::new)
            .insert(device.clone(), state);
        self.apply_and_commit_auxiliary(
            previous,
            candidate,
            "lighting",
            move |backend| backend.hardware.aura.apply(&apply_device, &apply_state),
            move |backend| backend.restore_lighting_device(&device, previous_device_state.as_ref()),
        )
    }

    pub fn release_fans(&mut self) -> Result<(), DaemonError> {
        self.persistence.require_writable()?;
        // Refuse before the transaction, so a refusal is not "compensated".
        self.hardware.check_fan_release_power()?;

        let previous = self.persisted.clone();
        let snapshot = self.fan_protection_snapshot();
        let mut candidate = previous.clone();
        // Direct mode requires curves, so releasing them returns to firmware.
        if let Some(desired) = candidate.desired.as_mut() {
            desired.fan_curves = None;
            desired.fan_mode = FanControlMode::Firmware;
        }
        candidate.state.fan_control_mode = FanControlMode::Firmware;
        candidate.state.fan_curves = None;
        candidate.state.overrides.fans = false;
        let rollback_state = previous.clone();
        self.apply_and_commit_auxiliary(
            previous,
            candidate,
            "fan release",
            |backend| {
                backend
                    .hardware
                    .fans_release()
                    .map_err(|error| error.to_string())
            },
            move |backend| backend.compensate_fan_release(&rollback_state, snapshot),
        )
    }

    pub fn shutdown(&mut self) {
        let (release, power_source) = shutdown_hardware_actions(&mut self.hardware);
        if let Err(error) = release {
            tracing::error!(%error, "failed to release EC control during shutdown");
        }
        self.suspended_on_battery = match power_source {
            Ok(on_battery) => Some(on_battery),
            Err(error) => {
                tracing::debug!(%error, "could not record power source before suspend");
                None
            }
        };
        if self.record_direct_release_failure("shutdown") {
            self.observe();
            let _ = self.save();
        }
    }

    /// Reapply every state that firmware may lose across suspend. Returns the
    /// current power source only when it differs from the source recorded
    /// before suspend, so the UI can immediately select its source-specific
    /// profile and panel policy.
    pub fn restore_volatile(&mut self) -> Option<bool> {
        let resumed_on_battery = self.hardware.sysfs.on_battery().ok();
        let power_source_changed =
            power_source_changed(self.suspended_on_battery, resumed_on_battery);
        self.suspended_on_battery = None;

        self.restore_desired("resume restore");
        self.restore_battery_policy();
        self.restore_panel_overdrive();
        self.restore_lighting();
        self.observe();
        if let Err(error) = self.save() {
            tracing::warn!(%error, "failed to persist post-resume state");
        }
        power_source_changed
    }

    pub fn restore_hotplugged_lighting(&mut self) {
        let connected = self.hardware.aura.take_reconnected();
        for device in connected {
            let state = self
                .persisted
                .state
                .devices
                .as_ref()
                .and_then(|devices| devices.get(&device))
                .cloned();
            if let Some(state) = state
                && let Err(error) = self.hardware.aura.apply(&device, &state)
            {
                tracing::warn!(%error, %device, "failed to relight hotplugged device");
                self.note_restore_failure(&format!("{device} lighting restore failed"), &error);
            }
        }
    }

    /// Refresh expensive platform and UI telemetry. Direct fan sampling is
    /// intentionally kept separate from the owner cadence. The snapshot is a
    /// read-only handoff from the 250 ms owner loop, not a request to drive it.
    /// Returns whether health (degraded or warnings) changed or a state
    /// change was committed, such as one-time charge completion.
    pub fn observe(&mut self) -> bool {
        let generation = self.persisted.state.generation;
        match self.hardware.direct.snapshot() {
            Ok(snapshot) => {
                self.hardware.sync_direct_snapshot(snapshot.clone());
                // The owner keeps its last error until direct mode is primed
                // again, so it is only current while direct mode is desired.
                let direct_desired = self
                    .persisted
                    .desired
                    .as_ref()
                    .is_some_and(|desired| desired.fan_mode == FanControlMode::Direct);
                if let Some(error) = snapshot.last_error
                    && direct_desired
                {
                    self.persisted.state.degraded = true;
                    push_warning(&mut self.persisted.state.warnings, error);
                }
            }
            Err(error) => {
                tracing::warn!(%error, "failed to observe direct fan runtime");
                self.persisted.state.degraded = true;
                push_warning(
                    &mut self.persisted.state.warnings,
                    format!("direct fan observation failed: {error}"),
                );
            }
        }
        self.record_direct_release_failure("direct fan observation");
        // An applied table is exact; the measured one cannot report PL2.
        if self.persisted.state.tdp.is_none()
            && let Ok(tdp) = self.hardware.sysfs.read_tdp()
        {
            self.persisted.state.tdp = Some(tdp);
        }
        if !self.persisted.state.battery_one_time_charge
            && let Ok(limit) = self.hardware.sysfs.battery_limit()
            && BATTERY_LIMIT_RANGE.contains(&limit)
        {
            self.persisted.state.battery_limit = Some(limit);
        }
        if let Ok(battery) = self.hardware.sysfs.battery_telemetry() {
            self.persisted.state.battery = battery;
        }
        self.complete_one_time_charge_if_full();
        if let Ok(value) = self.hardware.sysfs.read_panel_overdrive() {
            self.persisted.state.panel_overdrive = Some(value);
        }
        if let Some(temperature) = self.hardware.observed_temperature_c() {
            self.persisted.state.temperature = Some(temperature);
        }
        if let Ok(rpms) = sensors::read_fan_rpms() {
            self.persisted.state.fan_rpms = rpms;
        }
        self.hardware.aura.refresh();
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
            .map(TdpState::pl1_watts)
            .unwrap_or(0);
        self.persisted.state.direct_fan_duties = self.hardware.direct_snapshot.last_safe_duty;
        self.persisted.state.high_power_fan_protection =
            fan_protection_required(pl1, self.hardware.disable_high_power_fan_protection);
        self.persisted.state.telemetry = Telemetry {
            temperature_c: self.persisted.state.temperature,
            fan_rpms: self.persisted.state.fan_rpms,
        };
        let health = Health {
            degraded: self.persisted.state.degraded,
            warnings: self.persisted.state.warnings.clone(),
        };
        let changed =
            self.persisted.state.health != health || self.persisted.state.generation != generation;
        self.persisted.state.health = health;
        changed
    }

    fn save_state(&self, state: &PersistedState) -> Result<(), String> {
        self.persistence
            .require_writable()
            .map_err(|error| error.to_string())?;
        self.store.save(state)
    }

    fn save(&self) -> Result<(), String> {
        self.save_state(&self.persisted)
    }

    /// Run an auxiliary mutation as a small transaction. The candidate is
    /// complete except for its generation bump; hardware is changed first,
    /// then the candidate is published. Every caller supplies an explicit
    /// compensation operation because fan protection cannot use the same
    /// rollback policy as reversible scalar settings.
    fn apply_and_commit_auxiliary(
        &mut self,
        previous: PersistedState,
        mut candidate: PersistedState,
        context: &str,
        apply: impl FnOnce(&mut Self) -> Result<(), String>,
        rollback: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), DaemonError> {
        candidate.state.generation = candidate.state.generation.saturating_add(1);
        if let Err(error) = apply(self) {
            let rollback = rollback(self);
            return Err(self.auxiliary_failure(
                previous,
                context,
                DaemonError::Rejected(error),
                rollback,
            ));
        }
        self.commit_auxiliary(previous, candidate, context, rollback)
    }

    fn commit_auxiliary(
        &mut self,
        previous: PersistedState,
        candidate: PersistedState,
        context: &str,
        rollback: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), DaemonError> {
        if let Err(error) = self.save_state(&candidate) {
            let rollback = rollback(self);
            // StateStore can report an error after rename (for example when
            // syncing the parent directory). Restore the old durable state
            // before returning, or a restart could replay the rejected
            // candidate even when hardware compensation succeeded.
            self.persisted = previous.clone();
            let durable_rollback = self.save();
            let failure =
                self.auxiliary_failure(previous, context, DaemonError::Protocol(error), rollback);
            if let Err(durable_error) = durable_rollback {
                let message =
                    format!("{failure}; persistent state rollback failed: {durable_error}");
                self.note_restore_failure(
                    &format!("{context} persistent state rollback failed"),
                    &durable_error,
                );
                let _ = self.save();
                return Err(DaemonError::Protocol(message));
            }
            return Err(failure);
        }
        self.persisted = candidate;
        Ok(())
    }

    fn auxiliary_failure(
        &mut self,
        previous: PersistedState,
        context: &str,
        failure: DaemonError,
        rollback: Result<(), String>,
    ) -> DaemonError {
        self.persisted = previous;
        let Err(rollback_error) = rollback else {
            return failure;
        };

        let warning = format!("{context} rollback incomplete: {rollback_error}");
        self.note_restore_failure(context, &rollback_error);
        // Keep the known-good desired state and the degraded fact aligned on
        // disk when the compensation itself was incomplete. A restart must
        // not silently forget that hardware may still contain the candidate.
        let _ = self.save();
        let message = format!("{failure}; {warning}");
        if matches!(failure, DaemonError::Protocol(_)) {
            DaemonError::Protocol(message)
        } else {
            DaemonError::Rejected(message)
        }
    }

    fn restore_lighting_device(
        &mut self,
        device: &str,
        previous: Option<&LightingState>,
    ) -> Result<(), String> {
        // A detached device holds nothing to restore; hotplug relights it.
        self.hardware.aura.refresh();
        if !self.hardware.aura.available(device) {
            return Ok(());
        }
        let previous = previous
            .ok_or_else(|| format!("no captured known-good lighting state exists for {device}"))?;
        self.hardware.aura.apply(device, previous)
    }

    fn fan_protection_snapshot(&self) -> FanProtectionSnapshot {
        let request = self.persisted.desired.as_ref();
        FanProtectionSnapshot {
            curves: self
                .persisted
                .state
                .fan_curves
                .or_else(|| request.and_then(|request| request.fan_curves))
                .unwrap_or_else(|| {
                    z13helper_core::stock_fan_curves(self.hardware.ppd_profile.as_deref())
                }),
            hysteresis: request
                .map(|request| request.fan_hysteresis)
                .unwrap_or(self.persisted.state.fan_hysteresis),
            temperature_average_seconds: request
                .map(|request| request.fan_temperature_average_seconds)
                .unwrap_or(self.persisted.state.fan_temperature_average_seconds),
            disable_high_power_fan_protection: request
                .map(|request| request.disable_high_power_fan_protection)
                .unwrap_or(self.persisted.state.disable_high_power_fan_protection),
        }
    }

    /// Point the hardware fan policy, including the high-power override that
    /// release checks trust, back at the committed request. `set_fan_policy`
    /// records the fields even when the direct owner rejects the policy.
    fn restore_committed_fan_policy(&mut self) {
        let committed = self.fan_protection_snapshot();
        let _ = self.hardware.set_fan_policy(
            committed.hysteresis,
            committed.temperature_average_seconds,
            committed.disable_high_power_fan_protection,
        );
    }

    /// Reinstall protection without ever releasing direct EC control first.
    /// This is deliberately a firmware curve fallback: an explicit release
    /// may have left no safe direct target to restore.
    fn reinstall_safe_firmware_protection(
        &mut self,
        snapshot: FanProtectionSnapshot,
    ) -> Result<(), String> {
        let protected = [
            high_power_curve(&snapshot.curves[0]),
            high_power_curve(&snapshot.curves[1]),
        ];
        self.hardware.set_fan_policy(
            snapshot.hysteresis,
            snapshot.temperature_average_seconds,
            false,
        )?;
        self.hardware.sysfs.set_firmware_curves(&protected)?;
        self.hardware.release_direct()?;
        // Restoring the old override flag is safe only after the protected
        // copy is installed. The written hardware curve remains protected.
        self.hardware.disable_high_power_fan_protection =
            snapshot.disable_high_power_fan_protection;
        Ok(())
    }

    fn compensate_fan_release(
        &mut self,
        previous: &PersistedState,
        snapshot: FanProtectionSnapshot,
    ) -> Result<(), String> {
        let failure = if let Some(request) = previous.desired.as_ref()
            && request.fan_curves.is_some()
            && !request.disable_high_power_fan_protection
        {
            self.hardware.set_fan_policy(
                request.fan_hysteresis,
                request.fan_temperature_average_seconds,
                request.disable_high_power_fan_protection,
            )?;
            let Err(error) = set_fans(&mut self.hardware, request) else {
                return Ok(());
            };
            format!("previous fan control restoration failed ({error})")
        } else {
            "no safe known-good fan request was available".to_owned()
        };
        Err(match self.reinstall_safe_firmware_protection(snapshot) {
            Ok(()) => format!("{failure}; safer firmware protection installed"),
            Err(error) => format!("{failure}; safer protection reinstall failed ({error})"),
        })
    }

    fn restore_battery_policy(&mut self) {
        let target = effective_battery_limit(
            self.persisted.state.battery_one_time_charge,
            self.persisted.state.battery_limit,
        )
        .or_else(|| {
            // Nothing saved: only replace a reading that is not a real limit.
            matches!(self.hardware.sysfs.battery_limit(), Ok(limit) if !BATTERY_LIMIT_RANGE.contains(&limit))
                .then_some(100)
        });
        if let Some(target) = target
            && let Err(error) = self.hardware.sysfs.set_battery_limit(target)
        {
            tracing::warn!(%error, target, "failed to restore battery charge policy");
            self.note_restore_failure("battery policy restore failed", &error);
        }
    }

    fn restore_panel_overdrive(&mut self) {
        let Some(value) = self.persisted.state.panel_overdrive else {
            return;
        };
        if let Err(error) = self.hardware.sysfs.set_panel_overdrive(value != 0) {
            tracing::warn!(%error, value, "failed to restore panel overdrive");
            self.note_restore_failure("panel overdrive restore failed", &error);
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
        let previous = self.persisted.clone();
        let previous_hardware_limit = match self.hardware.sysfs.battery_limit() {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "could not capture battery limit before one-time restore");
                self.note_restore_failure("one-time battery charge restore failed", &error);
                return;
            }
        };
        let mut candidate = self.persisted.clone();
        candidate.state.battery_one_time_charge = false;
        if let Err(error) = self.apply_and_commit_auxiliary(
            previous,
            candidate,
            "one-time battery charge completion",
            move |backend| backend.hardware.sysfs.set_battery_limit(limit),
            move |backend| {
                backend
                    .hardware
                    .sysfs
                    .set_battery_limit(previous_hardware_limit)
            },
        ) {
            tracing::warn!(%error, "failed to persist completed one-time charge");
        }
    }

    fn restore_lighting(&mut self) {
        let lighting = self.persisted.state.devices.clone().unwrap_or_default();
        for (device, state) in lighting {
            if let Err(error) = self.restore_lighting_device(&device, Some(&state)) {
                tracing::warn!(%error, %device, "failed to restore lighting");
                self.note_restore_failure(&format!("{device} lighting restore failed"), &error);
            }
        }
    }

    /// Reapply the committed request at startup or resume. A failed restore
    /// releases direct EC control, since rollback replays the same request.
    fn restore_desired(&mut self, context: &str) {
        let Some(desired) = self.persisted.desired.clone() else {
            return;
        };
        if let Err(error) = self.apply(desired) {
            tracing::error!(%error, context, "failed to restore desired hardware state");
            self.note_restore_failure(&format!("{context} failed"), &error.to_string());
            let _ = self.hardware.release_direct();
            self.record_direct_release_failure(context);
        }
    }

    fn note_restore_failure(&mut self, context: &str, error: &str) {
        self.persisted.state.degraded = true;
        push_warning(
            &mut self.persisted.state.warnings,
            format!("{context}: {error}"),
        );
    }

    fn record_direct_release_failure(&mut self, context: &str) -> bool {
        let Some(error) = self.hardware.direct_release_failure.take() else {
            return false;
        };
        self.note_restore_failure(&format!("{context}: direct EC release failed"), &error);
        true
    }

    fn prevalidate(&self, request: &ApplyRequest) -> Result<(), DaemonError> {
        request.validate().map_err(DaemonError::Rejected)?;
        if let Some(profile) = request.ppd_profile.as_deref()
            && !self.hardware.ppd_profiles.is_empty()
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
        Ok(())
    }
}

fn loaded_state(
    loaded: Result<PersistedState, StateLoadError>,
) -> (PersistedState, PersistenceMode) {
    match loaded {
        Ok(state) => (state, PersistenceMode::ReadWrite),
        Err(StateLoadError::Unsupported(version)) => {
            tracing::warn!(
                version,
                "unsupported daemon state; starting without overwriting it"
            );
            let mut state = PersistedState::default();
            push_warning(
                &mut state.state.warnings,
                format!("unsupported state version {version}; expected 1"),
            );
            (state, PersistenceMode::UnsupportedReadOnly { version })
        }
        // Only a corrupt file that was preserved may be replaced.
        Err(StateLoadError::Corrupt(error)) => {
            tracing::warn!(%error, "starting from fresh daemon state");
            let mut state = PersistedState::default();
            push_warning(&mut state.state.warnings, error);
            (state, PersistenceMode::ReadWrite)
        }
        Err(StateLoadError::Io(error)) => {
            tracing::warn!(%error, "unreadable daemon state; starting without overwriting it");
            let mut state = PersistedState::default();
            push_warning(&mut state.state.warnings, error);
            (state, PersistenceMode::UnreadableReadOnly)
        }
    }
}

fn power_source_changed(before: Option<bool>, after: Option<bool>) -> Option<bool> {
    match (before, after) {
        (Some(before), Some(after)) if before != after => Some(after),
        (None, Some(after)) => Some(after),
        _ => None,
    }
}

fn factory_query_restore_plan(
    previous: Option<ApplyRequest>,
    original_ppd: Option<String>,
) -> Result<FactoryQueryRestore, DaemonError> {
    if let Some(previous) = previous {
        return Ok(FactoryQueryRestore::Desired(previous));
    }
    let profile = original_ppd.ok_or_else(|| {
        DaemonError::Rejected(
            "current PPD profile is unknown; refusing a query that cannot be restored".into(),
        )
    })?;
    let tdp = stock_tdp(Some(&profile)).ok_or_else(|| {
        DaemonError::Rejected(format!(
            "current PPD profile {profile:?} has no measured stock PPT table"
        ))
    })?;
    Ok(FactoryQueryRestore::PpdStock { profile, tdp })
}

fn effective_battery_limit(one_time_charge: bool, normal_limit: Option<i32>) -> Option<i32> {
    one_time_charge.then_some(100).or(normal_limit)
}

fn validate_factory_curve_query_power(tdp: TdpState) -> Result<(), DaemonError> {
    let pl1 = tdp.pl1_watts();
    if pl1 >= HIGH_POWER_THRESHOLD_W {
        return Err(DaemonError::Rejected(format!(
            "factory fan curves cannot be read while PL1 is {pl1} W; lower power first"
        )));
    }
    Ok(())
}

fn validate_fan_release_power(
    tdp: TdpState,
    disable_high_power_fan_protection: bool,
) -> Result<(), DaemonError> {
    let pl1 = tdp.pl1_watts();
    if fan_protection_required(pl1, disable_high_power_fan_protection) {
        return Err(DaemonError::Rejected(format!(
            "lower PL1 below {HIGH_POWER_THRESHOLD_W} W before releasing fan protection; current PL1 is {pl1} W"
        )));
    }
    Ok(())
}

fn normalize_factory_curve_profiles(profiles: Vec<String>) -> Result<Vec<String>, DaemonError> {
    if profiles.len() > MAX_FACTORY_CURVE_PROFILES * 4 {
        return Err(DaemonError::Rejected(format!(
            "at most {} factory fan-curve profiles may be requested",
            MAX_FACTORY_CURVE_PROFILES * 4
        )));
    }
    let mut unique = Vec::with_capacity(profiles.len().min(MAX_FACTORY_CURVE_PROFILES));
    let mut seen = HashSet::new();
    for profile in profiles {
        if !seen.insert(profile.clone()) {
            continue;
        }
        if unique.len() == MAX_FACTORY_CURVE_PROFILES {
            return Err(DaemonError::Rejected(format!(
                "at most {MAX_FACTORY_CURVE_PROFILES} unique factory fan-curve profiles may be requested"
            )));
        }
        unique.push(profile);
    }
    Ok(unique)
}

fn one_time_charge_is_complete(one_time_charge: bool, charge_percent: Option<u8>) -> bool {
    one_time_charge && charge_percent.is_some_and(|charge| charge >= 100)
}

fn validate_manual_undervolt(offset: i32, available: bool) -> Result<(), DaemonError> {
    validate_undervolt(offset).map_err(DaemonError::Rejected)?;
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
        Backend, FactoryQueryRestore, MAX_WARNINGS, PersistenceMode, PlatformHardware,
        ShutdownHardware, StateWriter, effective_battery_limit, factory_query_restore_plan,
        firmware_transition, loaded_state, normalize_factory_curve_profiles,
        one_time_charge_is_complete, power_source_changed, push_warning, shutdown_hardware_actions,
        stock_tdp, validate_factory_curve_query_power, validate_fan_release_power,
        validate_manual_undervolt,
    };
    use crate::aura::AuraDevices;
    use crate::direct_runtime::DirectRuntime;
    use crate::ec::{EcMailbox, PortIo};
    use crate::state::{PersistedState, StateLoadError};
    use crate::sysfs::Sysfs;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::fs;
    use std::io;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use z13helper_core::curve::Curve;
    use z13helper_core::error::DaemonError;
    use z13helper_core::profile::{FanControlMode, Profile};
    use z13helper_core::protocol::{ApplyRequest, LightingState, TdpState};

    #[derive(Clone, Default)]
    struct TransitionIo {
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    impl PortIo for TransitionIo {
        fn read_u8(&mut self, _port: u16) -> io::Result<u8> {
            Ok(0)
        }

        fn write_u8(&mut self, _port: u16, _value: u8) -> io::Result<()> {
            self.events.lock().unwrap().push("ec");
            Ok(())
        }
    }

    fn transition_curve() -> Curve {
        [[20, 100]; 8]
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum StoreFault {
        BeforePublish,
        AfterPublish,
    }

    #[derive(Clone)]
    struct FakeStateStore {
        durable: Arc<Mutex<PersistedState>>,
        fault: Arc<Mutex<Option<StoreFault>>>,
    }

    impl StateWriter for FakeStateStore {
        fn save(&self, state: &PersistedState) -> Result<(), String> {
            let fault = self.fault.lock().unwrap().take();
            if fault == Some(StoreFault::BeforePublish) {
                return Err("injected persistence failure before publish".into());
            }
            *self.durable.lock().unwrap() = state.clone();
            if fault == Some(StoreFault::AfterPublish) {
                Err("injected persistence failure after publish".into())
            } else {
                Ok(())
            }
        }
    }

    fn test_backend(persisted: PersistedState, store: Box<dyn StateWriter>) -> Backend {
        let controller = crate::service::Controller::new(EcMailbox::new(TransitionIo::default()));
        let (direct, probe) = DirectRuntime::fake(controller, || Ok(60_000));
        Backend {
            hardware: PlatformHardware {
                sysfs: Sysfs::new("/nonexistent/z13helper-test"),
                aura: AuraDevices::new(
                    "/nonexistent/z13helper-test",
                    "/nonexistent/z13helper-test",
                ),
                direct,
                direct_snapshot: super::DirectSnapshot::default(),
                latest_temperature_millic: None,
                fan_hysteresis: Default::default(),
                fan_temperature_average_seconds:
                    z13helper_core::profile::default_fan_temperature_average_seconds(),
                disable_high_power_fan_protection: false,
                undervolt_available: false,
                ppd_profiles: Vec::new(),
                ppd_profile: None,
                direct_release_failure: None,
            },
            persisted,
            store,
            persistence: PersistenceMode::ReadWrite,
            probe,
            suspended_on_battery: None,
        }
    }

    fn auxiliary_transaction_fixture(
        store_fault: Option<StoreFault>,
        fail_hardware: bool,
        fail_compensation: bool,
    ) -> (PersistedState, PersistedState, i32, Result<(), DaemonError>) {
        let mut previous = PersistedState::default();
        previous.state.generation = 7;
        let candidate = previous.clone();
        let durable = Arc::new(Mutex::new(previous.clone()));
        let fault = Arc::new(Mutex::new(store_fault));
        let store = FakeStateStore {
            durable: Arc::clone(&durable),
            fault,
        };
        let hardware_value = Arc::new(Mutex::new(0));
        let apply_value = Arc::clone(&hardware_value);
        let rollback_value = Arc::clone(&hardware_value);
        let mut backend = test_backend(previous.clone(), Box::new(store));
        let result = backend.apply_and_commit_auxiliary(
            previous,
            candidate,
            "test auxiliary",
            move |_| {
                *apply_value.lock().unwrap() = 1;
                if fail_hardware {
                    Err("injected hardware failure".into())
                } else {
                    Ok(())
                }
            },
            move |_| {
                if fail_compensation {
                    return Err("injected compensation failure".into());
                }
                *rollback_value.lock().unwrap() = 0;
                Ok(())
            },
        );
        let persisted = backend.persisted.clone();
        let durable = durable.lock().unwrap().clone();
        let hardware = *hardware_value.lock().unwrap();
        drop(backend);
        (persisted, durable, hardware, result)
    }

    fn memory_store() -> Box<dyn StateWriter> {
        Box::new(FakeStateStore {
            durable: Default::default(),
            fault: Default::default(),
        })
    }

    /// 90 W direct-mode request without PPD, so no D-Bus write is attempted.
    /// The test sysfs root does not exist, so the PPT write always fails.
    fn high_power_request(disable_high_power_fan_protection: bool) -> ApplyRequest {
        let pl1 = 90;
        ApplyRequest {
            ppd_profile: None,
            power_limits: Some(TdpState {
                pl1_spl: pl1,
                pl2_sppt: pl1,
                fppt: pl1,
                apu_sppt: pl1,
                platform_sppt: pl1,
            }),
            fan_mode: FanControlMode::Direct,
            fan_curves: Some(z13helper_core::stock_fan_curves(None)),
            undervolt: None,
            cpu_temp_limit: z13helper_core::profile::default_cpu_temp_limit(),
            fan_hysteresis: Default::default(),
            fan_temperature_average_seconds:
                z13helper_core::profile::default_fan_temperature_average_seconds(),
            disable_high_power_fan_protection,
        }
    }

    #[test]
    fn executor_rejects_socket_only_commands_without_panicking() {
        use z13helper_core::error::ErrorCode;
        use z13helper_core::protocol::{ClientId, Command, RequestId};
        let mut backend = test_backend(PersistedState::default(), memory_store());
        let request_id = RequestId::new(1).unwrap();
        for command in [
            Command::SetControllerCapture { enabled: true },
            Command::Subscribe { events: vec![] },
            Command::GetOutcome {
                target_client_id: ClientId::new(1).unwrap(),
                target_request_id: request_id,
            },
        ] {
            let response = crate::protocol::execute(&mut backend, request_id, command);
            assert_eq!(response.error.unwrap().code, ErrorCode::Protocol);
        }
    }

    #[test]
    fn rejected_override_apply_keeps_committed_fan_protection() {
        let mut backend = test_backend(PersistedState::default(), memory_store());
        assert!(backend.apply(high_power_request(true)).is_err());
        assert!(!backend.hardware.disable_high_power_fan_protection);
    }

    #[test]
    fn failed_restore_releases_direct_ec_control() {
        let persisted = PersistedState {
            desired: Some(high_power_request(false)),
            ..Default::default()
        };
        let mut backend = test_backend(persisted, memory_store());
        backend.restore_desired("startup restore");
        assert!(backend.persisted.state.degraded);
        assert!(!backend.hardware.direct.snapshot().unwrap().enabled);
    }

    #[test]
    fn direct_failure_degrades_only_while_direct_mode_is_desired() {
        let persisted = PersistedState {
            desired: Some(high_power_request(false)),
            ..Default::default()
        };
        let mut backend = test_backend(persisted, memory_store());
        let fail = Arc::new(AtomicBool::new(false));
        let sensor_fail = Arc::clone(&fail);
        let controller = crate::service::Controller::new(EcMailbox::new(TransitionIo::default()));
        (backend.hardware.direct, _) = DirectRuntime::fake(controller, move || {
            if sensor_fail.load(Ordering::SeqCst) {
                Err("fake sensor failed".into())
            } else {
                Ok(60_000)
            }
        });
        backend
            .hardware
            .direct
            .install_and_prime([transition_curve(), transition_curve()])
            .unwrap();
        fail.store(true, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(350));

        assert!(backend.observe(), "a new direct failure changes health");
        assert!(backend.persisted.state.degraded);
        assert!(!backend.observe(), "unchanged health is not republished");

        // What a later successful firmware-mode apply leaves behind.
        backend.persisted.desired.as_mut().unwrap().fan_mode = FanControlMode::Firmware;
        backend.persisted.state.degraded = false;
        backend.persisted.state.warnings.clear();
        backend.observe();
        assert!(!backend.persisted.state.degraded);
        assert!(backend.persisted.state.warnings.is_empty());
    }

    #[test]
    fn observe_reports_committed_one_time_charge_completion() {
        let root = std::env::temp_dir().join(format!(
            "z13helper-one-time-charge-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let battery = root.join("class/power_supply/BAT0");
        fs::create_dir_all(&battery).unwrap();
        fs::write(battery.join("capacity"), "100\n").unwrap();
        fs::write(battery.join("status"), "Full\n").unwrap();
        fs::write(battery.join("charge_control_end_threshold"), "100\n").unwrap();
        let mut persisted = PersistedState::default();
        persisted.state.battery_one_time_charge = true;
        persisted.state.battery_limit = Some(80);
        let mut backend = test_backend(persisted, memory_store());
        backend.hardware.sysfs = Sysfs::new(&root);

        assert!(backend.observe(), "completion changes persisted state");
        assert!(!backend.persisted.state.battery_one_time_charge);
        assert_eq!(backend.hardware.sysfs.battery_limit(), Ok(80));
        assert!(!backend.observe(), "nothing new is not republished");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unreadable_or_unpreserved_state_is_never_overwritten() {
        let (_, mode) = loaded_state(Err(StateLoadError::Io("read state.json: EIO".into())));
        assert!(mode.require_writable().is_err());
        let (state, mode) = loaded_state(Err(StateLoadError::Corrupt("preserved".into())));
        assert!(mode.require_writable().is_ok());
        assert_eq!(state.state.warnings, ["preserved"]);
        let (_, mode) = loaded_state(Err(StateLoadError::Unsupported(99)));
        assert!(mode.require_writable().is_err());
    }

    #[test]
    fn detached_lighting_device_is_not_reported_degraded() {
        let mut backend = test_backend(PersistedState::default(), memory_store());
        let state = LightingState::default();
        assert!(
            backend
                .set_lighting("keyboard".into(), state.clone())
                .is_err()
        );
        assert!(!backend.persisted.state.degraded);
        backend.persisted.state.devices = Some(HashMap::from([("keyboard".into(), state)]));
        backend.restore_lighting();
        assert!(!backend.persisted.state.degraded);
    }

    fn armoury_fixture(root: &std::path::Path, watts: i32) {
        for name in ["ppt_pl1_spl", "ppt_pl2_sppt", "ppt_pl3_fppt"] {
            let attribute = root
                .join("class/firmware-attributes/asus-armoury/attributes")
                .join(name);
            fs::create_dir_all(&attribute).unwrap();
            fs::write(attribute.join("current_value"), format!("{watts}\n")).unwrap();
        }
    }

    /// Fake sysfs root whose battery reports `threshold` as its charge limit.
    fn battery_fixture(name: &str, threshold: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "z13helper-battery-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let battery = root.join("class/power_supply/BAT0");
        fs::create_dir_all(&battery).unwrap();
        let file = battery.join("charge_control_end_threshold");
        fs::write(&file, threshold).unwrap();
        (root, file)
    }

    #[test]
    fn observe_ignores_an_out_of_range_external_battery_limit() {
        // Some kernels/tools (seen on Bazzite) report 0; it is never a real limit.
        let (root, _) = battery_fixture("observe", "0\n");
        let mut persisted = PersistedState::default();
        persisted.state.battery_limit = Some(80);
        let mut backend = test_backend(persisted, memory_store());
        backend.hardware.sysfs = Sysfs::new(&root);

        backend.observe();

        assert_eq!(backend.persisted.state.battery_limit, Some(80));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn restore_without_a_saved_limit_only_replaces_an_invalid_one() {
        for (threshold, restored) in [("0\n", "100\n"), ("80\n", "80\n")] {
            let (root, file) = battery_fixture("restore", threshold);
            let mut backend = test_backend(PersistedState::default(), memory_store());
            backend.hardware.sysfs = Sysfs::new(&root);

            backend.restore_battery_policy();

            assert_eq!(fs::read_to_string(&file).unwrap(), restored);
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn observe_keeps_the_exact_applied_power_table() {
        let root = std::env::temp_dir().join(format!(
            "z13helper-observe-tdp-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        // Armoury holds the firmware-clamped copy of a sub-minimum request.
        armoury_fixture(&root, 32);
        let applied = TdpState {
            pl1_spl: 18,
            pl2_sppt: 18,
            fppt: 93,
            apu_sppt: 18,
            platform_sppt: 18,
        };
        let mut persisted = PersistedState::default();
        persisted.state.tdp = Some(applied);
        let mut backend = test_backend(persisted, memory_store());
        backend.hardware.sysfs = Sysfs::new(&root);

        backend.observe();

        assert_eq!(backend.persisted.state.tdp, Some(applied));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn released_fans_return_the_desired_request_to_firmware_mode() {
        let root = std::env::temp_dir().join(format!(
            "z13helper-release-fans-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let hwmon = root.join("class/hwmon/hwmon0");
        armoury_fixture(&root, 50);
        fs::create_dir_all(&hwmon).unwrap();
        fs::write(hwmon.join("name"), "asus_custom_fan_curve\n").unwrap();
        let mut request = high_power_request(false);
        request.power_limits = None;
        let mut persisted = PersistedState {
            desired: Some(request),
            ..Default::default()
        };
        persisted.state.fan_control_mode = FanControlMode::Direct;
        let mut backend = test_backend(persisted, memory_store());
        backend.hardware.sysfs = Sysfs::new(&root);

        backend.release_fans().unwrap();

        assert_eq!(
            backend.persisted.state.fan_control_mode,
            FanControlMode::Firmware
        );
        let desired = backend.persisted.desired.clone().unwrap();
        assert_eq!(desired.fan_mode, FanControlMode::Firmware);
        assert!(desired.validate().is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn auxiliary_success_publishes_candidate_after_hardware_apply() {
        let (persisted, durable, hardware, result) =
            auxiliary_transaction_fixture(None, false, false);
        assert!(result.is_ok());
        assert_eq!(persisted.state.generation, 8);
        assert_eq!(durable.state.generation, 8);
        assert_eq!(hardware, 1);
    }

    #[test]
    fn partial_auxiliary_write_is_compensated_at_each_failure_index() {
        for fail_at in 0..5 {
            let previous = PersistedState::default();
            let candidate = previous.clone();
            let durable = Arc::new(Mutex::new(previous.clone()));
            let store = FakeStateStore {
                durable: Arc::clone(&durable),
                fault: Arc::new(Mutex::new(None)),
            };
            let events = Arc::new(Mutex::new(Vec::new()));
            let apply_events = Arc::clone(&events);
            let rollback_events = Arc::clone(&events);
            let mut backend = test_backend(previous.clone(), Box::new(store));
            let result = backend.apply_and_commit_auxiliary(
                previous,
                candidate,
                "partial write",
                move |_| {
                    for index in 0..5 {
                        apply_events.lock().unwrap().push(index);
                        if index == fail_at {
                            return Err(format!("injected write failure at node {index}"));
                        }
                    }
                    Ok(())
                },
                move |_| {
                    rollback_events.lock().unwrap().push(100 + fail_at);
                    Ok(())
                },
            );
            assert!(result.is_err(), "failure index {fail_at}");
            assert_eq!(
                backend.persisted.state.generation, 0,
                "failure index {fail_at}"
            );
            assert_eq!(
                durable.lock().unwrap().state.generation,
                0,
                "failure index {fail_at}"
            );
            assert_eq!(
                *events.lock().unwrap(),
                (0..=fail_at)
                    .chain(std::iter::once(100 + fail_at))
                    .collect::<Vec<_>>(),
                "failure index {fail_at}"
            );
            drop(backend);
        }
    }

    #[test]
    fn auxiliary_compensation_failure_is_degraded_but_keeps_previous_durable_state() {
        let (persisted, durable, hardware, result) =
            auxiliary_transaction_fixture(Some(StoreFault::BeforePublish), false, true);
        assert!(result.is_err());
        assert_eq!(persisted.state.generation, 7);
        assert!(persisted.state.degraded);
        assert!(!persisted.state.warnings.is_empty());
        assert_eq!(durable.state.generation, 7);
        assert!(durable.state.degraded);
        assert_eq!(hardware, 1);
    }

    #[test]
    fn auxiliary_persistence_failure_rolls_back_before_and_after_publish() {
        for fault in [StoreFault::BeforePublish, StoreFault::AfterPublish] {
            let (persisted, durable, hardware, result) =
                auxiliary_transaction_fixture(Some(fault), false, false);
            assert!(matches!(result, Err(DaemonError::Protocol(_))));
            assert_eq!(persisted.state.generation, 7, "fault {fault:?}");
            assert_eq!(durable.state.generation, 7, "fault {fault:?}");
            assert_eq!(hardware, 0, "fault {fault:?}");
        }
    }

    #[test]
    fn factory_query_restoration_is_explicit_about_ppd_normalization_without_desired_state() {
        let request = ApplyRequest::from_profile(&Profile::builtin("balanced", "Balanced"), false);
        assert!(matches!(
            factory_query_restore_plan(Some(request), Some("balanced".into())),
            Ok(FactoryQueryRestore::Desired(_))
        ));
        assert!(matches!(
            factory_query_restore_plan(None, Some("balanced".into())),
            Ok(FactoryQueryRestore::PpdStock { profile, tdp })
                if profile == "balanced" && tdp == stock_tdp(Some("balanced")).unwrap()
        ));
        assert!(factory_query_restore_plan(None, None).is_err());
    }

    #[test]
    fn one_time_charge_overrides_and_then_restores_the_normal_limit() {
        assert_eq!(effective_battery_limit(false, Some(80)), Some(80));
        assert_eq!(effective_battery_limit(true, Some(80)), Some(100));
        assert!(!one_time_charge_is_complete(true, Some(99)));
        assert!(one_time_charge_is_complete(true, Some(100)));
        assert!(!one_time_charge_is_complete(false, Some(100)));
    }

    #[test]
    fn firmware_transition_releases_ec_only_after_confirmed_curve_write() {
        let io = TransitionIo::default();
        let events = Arc::clone(&io.events);
        let controller = crate::service::Controller::new(EcMailbox::new(io));
        let (mut direct, _) = DirectRuntime::fake(controller, || Ok(60_000));
        direct
            .install_and_prime([transition_curve(), transition_curve()])
            .unwrap();
        events.lock().unwrap().clear();

        firmware_transition(&mut direct, || {
            events.lock().unwrap().push("firmware-confirmed");
            Ok(())
        })
        .unwrap();

        let events = events.lock().unwrap();
        assert_eq!(events.first(), Some(&"firmware-confirmed"));
        assert!(events.iter().skip(1).any(|event| *event == "ec"));
        assert!(!direct.snapshot().unwrap().enabled);
    }

    #[test]
    fn failed_firmware_curve_write_resumes_direct_without_releasing_ec() {
        let io = TransitionIo::default();
        let events = Arc::clone(&io.events);
        let controller = crate::service::Controller::new(EcMailbox::new(io));
        let (mut direct, _) = DirectRuntime::fake(controller, || Ok(60_000));
        direct
            .install_and_prime([transition_curve(), transition_curve()])
            .unwrap();
        events.lock().unwrap().clear();

        let error = firmware_transition(&mut direct, || {
            events.lock().unwrap().push("firmware-failed");
            Err("firmware write failed".into())
        })
        .unwrap_err();

        assert_eq!(error, "firmware write failed");
        assert_eq!(*events.lock().unwrap(), ["firmware-failed"]);
        let snapshot = direct.snapshot().unwrap();
        assert!(snapshot.enabled);
        assert!(!snapshot.held);
        assert_eq!(snapshot.last_safe_duty, [100, 100]);
    }

    struct ShutdownFake {
        actions: Rc<RefCell<Vec<&'static str>>>,
    }

    impl ShutdownHardware for ShutdownFake {
        fn release_direct_for_shutdown(&mut self) -> Result<(), String> {
            self.actions.borrow_mut().push("release-direct");
            Ok(())
        }

        fn on_battery_for_shutdown(&self) -> Result<bool, String> {
            self.actions.borrow_mut().push("power-source");
            Ok(false)
        }
    }

    #[test]
    fn shutdown_sequence_releases_direct_control_before_power_observation() {
        let actions = Rc::new(RefCell::new(Vec::new()));
        let mut fake = ShutdownFake {
            actions: Rc::clone(&actions),
        };
        let _ = shutdown_hardware_actions(&mut fake);
        assert_eq!(*actions.borrow(), ["release-direct", "power-source"]);
    }

    #[test]
    fn factory_curve_query_never_drops_high_power_protection() {
        let tdp = |pl1| TdpState {
            pl1_spl: pl1,
            ..TdpState::default()
        };
        assert!(validate_factory_curve_query_power(tdp(79)).is_ok());
        assert!(validate_factory_curve_query_power(tdp(80)).is_err());
    }

    #[test]
    fn fan_release_requires_low_actual_power_unless_override_is_confirmed() {
        let tdp = |pl1| TdpState {
            pl1_spl: pl1,
            ..TdpState::default()
        };
        assert!(validate_fan_release_power(tdp(79), false).is_ok());
        assert!(validate_fan_release_power(tdp(80), false).is_err());
        assert!(validate_fan_release_power(tdp(120), true).is_ok());
    }

    #[test]
    fn one_shot_undervolt_requires_a_safe_available_offset() {
        assert!(validate_manual_undervolt(-20, true).is_ok());
        assert!(validate_manual_undervolt(-41, true).is_err());
        assert!(validate_manual_undervolt(-20, false).is_err());
    }

    #[test]
    fn resume_reports_only_a_real_power_source_transition() {
        assert_eq!(power_source_changed(Some(false), Some(true)), Some(true));
        assert_eq!(power_source_changed(Some(true), Some(false)), Some(false));
        assert_eq!(power_source_changed(Some(false), Some(false)), None);
        assert_eq!(power_source_changed(Some(true), None), None);
        assert_eq!(power_source_changed(None, Some(false)), Some(false));
    }

    #[test]
    fn warnings_are_deduplicated_and_bounded() {
        let mut warnings = Vec::new();
        for index in 0..(MAX_WARNINGS + 4) {
            push_warning(&mut warnings, format!("warning {index}"));
        }
        push_warning(&mut warnings, "warning 99");
        push_warning(&mut warnings, "warning 99");
        assert_eq!(warnings.len(), MAX_WARNINGS);
        push_warning(&mut warnings, "x".repeat(2_048));
        assert_eq!(warnings.last().unwrap().chars().count(), 1_024);
    }

    #[test]
    fn factory_curve_requests_are_deduplicated_and_bounded() {
        assert_eq!(
            normalize_factory_curve_profiles(vec![
                "silent".into(),
                "silent".into(),
                "turbo".into(),
            ])
            .unwrap(),
            vec!["silent".to_owned(), "turbo".to_owned()]
        );
        let profiles = (0..9).map(|index| index.to_string()).collect();
        assert!(normalize_factory_curve_profiles(profiles).is_err());
    }
}
