use std::fs;
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use z13helper_core::curve::{Curve, validate};
use z13helper_core::protocol::{BatteryTelemetry, TdpState};

const STRIX_HALO_TCTL_COMMAND: u32 = 0x19;
const STRIX_HALO_CHTC_COMMAND: u32 = 0x63;
// Strix Halo is in RyzenAdj's modern mobile-family command group. The
// 0x4f/0x3e/0x5f group is for Dragon/Firerange and does not update these
// limits on this platform.
const STRIX_HALO_STAPM_COMMAND: u32 = 0x14;
const STRIX_HALO_FAST_LIMIT_COMMAND: u32 = 0x15;
const STRIX_HALO_SLOW_LIMIT_COMMAND: u32 = 0x16;
const STRIX_HALO_APU_SLOW_LIMIT_COMMAND: u32 = 0x23;
const STRIX_HALO_STAPM_PM_TABLE_OFFSET: usize = 0;
const STRIX_HALO_FAST_LIMIT_PM_TABLE_OFFSET: usize = 2 * size_of::<f32>();
const STRIX_HALO_SLOW_LIMIT_PM_TABLE_OFFSET: usize = 4 * size_of::<f32>();
const STRIX_HALO_APU_SLOW_LIMIT_PM_TABLE_OFFSET: usize = 6 * size_of::<f32>();
const STRIX_HALO_TCTL_PM_TABLE_OFFSET: usize = 22 * size_of::<f32>();
const STRIX_HALO_POWER_LIMIT_SETTLE_TIMEOUT: Duration = Duration::from_millis(500);
const STRIX_HALO_POWER_LIMIT_SETTLE_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Clone, Debug)]
pub struct Sysfs {
    root: PathBuf,
}

impl Default for Sysfs {
    fn default() -> Self {
        Self::new("/sys")
    }
}

impl Sysfs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, absolute: &str) -> PathBuf {
        self.root.join(absolute.trim_start_matches("/sys/"))
    }

    fn read_text(&self, path: impl AsRef<Path>) -> Result<String, String> {
        fs::read_to_string(path.as_ref())
            .map(|value| value.trim().to_owned())
            .map_err(|error| format!("read {}: {error}", path.as_ref().display()))
    }

    fn write_text(
        &self,
        path: impl AsRef<Path>,
        value: impl std::fmt::Display,
    ) -> Result<(), String> {
        fs::write(path.as_ref(), format!("{value}\n"))
            .map_err(|error| format!("write {}: {error}", path.as_ref().display()))
    }

    fn write_and_verify(&self, path: impl AsRef<Path>, value: i32) -> Result<(), String> {
        self.write_text(&path, value)?;
        let actual = self.read_text(&path)?;
        if actual != value.to_string() {
            return Err(format!(
                "verify {}: wrote {value}, read {actual:?}",
                path.as_ref().display()
            ));
        }
        Ok(())
    }

    fn ppt_path(&self, name: &str) -> PathBuf {
        self.path("/sys/devices/platform/asus-nb-wmi").join(name)
    }

    pub fn set_tdp(&self, state: TdpState) -> Result<(), String> {
        for (name, value) in [
            ("ppt_pl1_spl", state.pl1_spl),
            ("ppt_pl2_sppt", state.pl2_sppt),
            ("ppt_fppt", state.fppt),
            ("ppt_apu_sppt", state.apu_sppt),
            ("ppt_platform_sppt", state.platform_sppt),
        ] {
            self.write_text(self.ppt_path(name), value)?;
        }
        // The ASUS WMI attributes are writable echoes on this platform. They
        // do not reliably update the effective SMU limits after PPD changes,
        // so reassert the limits through the same MP1 interface used by
        // ryzenadj and verify the PM table before reporting success.
        if self.smu_path("mp1_smu_cmd").exists() && self.smu_path("pm_table").exists() {
            self.set_strix_halo_tdp(state)?;
        }
        Ok(())
    }

    pub fn read_tdp(&self) -> Result<TdpState, String> {
        let read = |name: &str| -> Result<i32, String> {
            self.read_text(self.ppt_path(name))?
                .parse()
                .map_err(|error| format!("parse {name}: {error}"))
        };
        let state = TdpState {
            pl1_spl: read("ppt_pl1_spl")?,
            pl2_sppt: read("ppt_pl2_sppt")?,
            fppt: read("ppt_fppt")?,
            apu_sppt: read("ppt_apu_sppt")?,
            platform_sppt: read("ppt_platform_sppt")?,
        };
        Ok(state)
    }

    fn find_hwmon(&self, name: &str) -> Result<PathBuf, String> {
        let directory = self.path("/sys/class/hwmon");
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("read {}: {error}", directory.display()))?
            .flatten()
        {
            if self.read_text(entry.path().join("name")).as_deref() == Ok(name) {
                return Ok(entry.path());
            }
        }
        Err(format!("hwmon device {name:?} not found"))
    }

    pub fn set_firmware_curves(&self, curves: &[Curve; 2]) -> Result<(), String> {
        for curve in curves {
            validate(curve).map_err(|error| error.to_string())?;
        }
        let curve_dir = self.find_hwmon("asus_custom_fan_curve")?;
        for (fan, curve) in curves.iter().enumerate() {
            let index = fan + 1;
            for (point, [temperature, duty]) in curve.iter().copied().enumerate() {
                self.write_text(
                    curve_dir.join(format!("pwm{index}_auto_point{}_temp", point + 1)),
                    temperature,
                )?;
                self.write_text(
                    curve_dir.join(format!("pwm{index}_auto_point{}_pwm", point + 1)),
                    duty,
                )?;
            }
            self.write_and_verify(curve_dir.join(format!("pwm{index}_enable")), 1)?;
        }
        Ok(())
    }

    pub fn release_firmware_fans(&self) -> Result<(), String> {
        let curve_dir = self.find_hwmon("asus_custom_fan_curve")?;
        for index in 1..=2 {
            self.write_and_verify(curve_dir.join(format!("pwm{index}_enable")), 2)?;
        }
        Ok(())
    }

    /// Ask the ASUS WMI driver to reload both factory curve tables for the
    /// currently selected firmware profile, then read its cached points.
    /// Writing mode 3 intentionally leaves each curve in firmware-auto mode.
    pub fn factory_fan_curves(&self) -> Result<[Curve; 2], String> {
        let curve_dir = self.find_hwmon("asus_custom_fan_curve")?;
        let mut curves = [[[0; 2]; 8]; 2];
        for (fan, curve) in curves.iter_mut().enumerate() {
            let index = fan + 1;
            self.write_text(curve_dir.join(format!("pwm{index}_enable")), 3)?;
            for (point, values) in curve.iter_mut().enumerate() {
                let point = point + 1;
                values[0] = self
                    .read_text(curve_dir.join(format!("pwm{index}_auto_point{point}_temp")))?
                    .parse()
                    .map_err(|error| format!("parse fan {index} point {point} temp: {error}"))?;
                values[1] = self
                    .read_text(curve_dir.join(format!("pwm{index}_auto_point{point}_pwm")))?
                    .parse()
                    .map_err(|error| format!("parse fan {index} point {point} PWM: {error}"))?;
            }
            validate(curve).map_err(|error| format!("factory fan {index} curve: {error}"))?;
        }
        Ok(curves)
    }

    pub fn battery_limit(&self) -> Result<i32, String> {
        let directory = self.path("/sys/class/power_supply");
        for entry in fs::read_dir(&directory).into_iter().flatten().flatten() {
            let path = entry.path().join("charge_control_end_threshold");
            if entry.file_name().to_string_lossy().starts_with("BAT") && path.exists() {
                return self
                    .read_text(path)?
                    .parse()
                    .map_err(|error| format!("parse battery limit: {error}"));
            }
        }
        Err("battery charge threshold not found".into())
    }

    pub fn battery_telemetry(&self) -> Result<BatteryTelemetry, String> {
        let directory = self.path("/sys/class/power_supply");
        for entry in fs::read_dir(&directory).into_iter().flatten().flatten() {
            if !entry.file_name().to_string_lossy().starts_with("BAT") {
                continue;
            }
            let charge_percent = self
                .read_text(entry.path().join("capacity"))?
                .parse::<u8>()
                .map_err(|error| format!("parse battery capacity: {error}"))?;
            if charge_percent > 100 {
                return Err(format!(
                    "battery capacity must be between 0 and 100, got {charge_percent}"
                ));
            }
            let status = self.read_text(entry.path().join("status"))?;
            let read_measurement = |name: &str| {
                self.read_text(entry.path().join(name))
                    .ok()?
                    .parse::<i64>()
                    .ok()
                    .map(i64::unsigned_abs)
            };
            let power_microwatts = read_measurement("power_now").or_else(|| {
                let current = read_measurement("current_now")?;
                let voltage = read_measurement("voltage_now")?;
                current.checked_mul(voltage).map(|value| value / 1_000_000)
            });
            let health_percent = [
                ("energy_full", "energy_full_design"),
                ("charge_full", "charge_full_design"),
            ]
            .into_iter()
            .find_map(|(full, design)| {
                battery_health_percent(read_measurement(full)?, read_measurement(design)?)
            });
            return Ok(BatteryTelemetry {
                charge_percent: Some(charge_percent),
                status: (!status.is_empty()).then_some(status),
                power_microwatts,
                health_percent,
            });
        }
        Err("battery telemetry not found".into())
    }

    /// Return the current power source as `true` for battery power.
    ///
    /// The adapter's online state is authoritative when available. Battery
    /// status is only a fallback because a charging battery can still be
    /// connected to AC.
    pub fn on_battery(&self) -> Result<bool, String> {
        let directory = self.path("/sys/class/power_supply");
        let mut found_adapter = false;
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("read {}: {error}", directory.display()))?
            .flatten()
        {
            let kind = self.read_text(entry.path().join("type"))?;
            if kind != "Mains" && kind != "ADP" {
                continue;
            }
            found_adapter = true;
            let online = self
                .read_text(entry.path().join("online"))?
                .parse::<u8>()
                .map_err(|error| format!("parse adapter online state: {error}"))?;
            if online == 1 {
                return Ok(false);
            }
        }
        if found_adapter {
            return Ok(true);
        }

        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("read {}: {error}", directory.display()))?
            .flatten()
        {
            if self.read_text(entry.path().join("type"))? != "Battery" {
                continue;
            }
            return Ok(self.read_text(entry.path().join("status"))? == "Discharging");
        }
        Err("power source not found".into())
    }

    pub fn set_battery_limit(&self, limit: i32) -> Result<(), String> {
        if !(40..=100).contains(&limit) {
            return Err("battery limit must be between 40 and 100".into());
        }
        let directory = self.path("/sys/class/power_supply");
        for entry in fs::read_dir(&directory).into_iter().flatten().flatten() {
            let path = entry.path().join("charge_control_end_threshold");
            if entry.file_name().to_string_lossy().starts_with("BAT") && path.exists() {
                return self.write_text(path, limit);
            }
        }
        Err("battery charge threshold not found".into())
    }

    fn armoury_attribute(&self, name: &str) -> PathBuf {
        self.path("/sys/class/firmware-attributes/asus-armoury/attributes")
            .join(name)
            .join("current_value")
    }

    pub fn read_armoury_bool(&self, name: &str) -> Result<i32, String> {
        self.read_text(self.armoury_attribute(name))?
            .parse()
            .map_err(|error| format!("parse {name}: {error}"))
    }

    pub fn set_armoury_bool(&self, name: &str, enabled: bool) -> Result<(), String> {
        self.write_text(self.armoury_attribute(name), i32::from(enabled))
    }

    fn smu_path(&self, name: &str) -> PathBuf {
        self.path("/sys/kernel/ryzen_smu_drv").join(name)
    }

    fn send_smu_co(&self, offset: i32) -> Result<(), String> {
        let args = encode_smu_co(offset)?;
        self.send_smu_command(0x4C, args, "Curve Optimizer")
    }

    fn send_smu_command(
        &self,
        command: u32,
        args: [u8; 24],
        operation: &str,
    ) -> Result<(), String> {
        fs::write(self.smu_path("smu_args"), args)
            .map_err(|error| format!("write smu_args: {error}"))?;
        fs::write(self.smu_path("mp1_smu_cmd"), command.to_le_bytes())
            .map_err(|error| format!("write mp1_smu_cmd: {error}"))?;
        let response = fs::read(self.smu_path("mp1_smu_cmd"))
            .map_err(|error| format!("read mp1_smu_cmd: {error}"))?;
        if response.len() < 4 || u32::from_le_bytes(response[..4].try_into().unwrap()) != 1 {
            return Err(format!("SMU rejected {operation} command"));
        }
        Ok(())
    }

    pub fn probe_undervolt_once(&self) -> bool {
        self.smu_path("mp1_smu_cmd").exists() && self.send_smu_co(0).is_ok()
    }

    pub fn set_undervolt(&self, offset: i32) -> Result<(), String> {
        self.send_smu_co(offset)
    }

    pub fn set_cpu_temp_limit(&self, temperature_c: u8) -> Result<(), String> {
        let args = encode_smu_temp_limit(temperature_c)?;
        self.send_smu_command(STRIX_HALO_TCTL_COMMAND, args, "APU Tctl limit")?;
        self.send_smu_command(STRIX_HALO_CHTC_COMMAND, args, "APU cHTC limit")?;
        let effective = self.read_cpu_temp_limit()?;
        if effective != temperature_c {
            return Err(format!(
                "APU temperature-limit verification failed: requested {temperature_c}°C, firmware reports {effective}°C"
            ));
        }
        Ok(())
    }

    fn read_cpu_temp_limit(&self) -> Result<u8, String> {
        let table = fs::read(self.smu_path("pm_table"))
            .map_err(|error| format!("read SMU PM table: {error}"))?;
        decode_strix_halo_tctl(&table)
    }

    fn set_strix_halo_tdp(&self, state: TdpState) -> Result<(), String> {
        let expected = [
            (STRIX_HALO_STAPM_COMMAND, state.pl2_sppt, "STAPM limit"),
            (STRIX_HALO_FAST_LIMIT_COMMAND, state.fppt, "PPT fast limit"),
            (
                STRIX_HALO_SLOW_LIMIT_COMMAND,
                state.pl1_spl,
                "PPT slow limit",
            ),
            (
                STRIX_HALO_APU_SLOW_LIMIT_COMMAND,
                state.apu_sppt,
                "APU PPT limit",
            ),
        ];

        let mut last_error = String::from("SMU power-limit verification failed");
        for attempt in 0..3 {
            let result = (|| {
                for &(command, watts, operation) in &expected {
                    let milliwatts = u32::try_from(watts)
                        .ok()
                        .and_then(|watts| watts.checked_mul(1_000))
                        .ok_or_else(|| format!("{operation} is outside the SMU range"))?;
                    self.send_smu_command(command, encode_smu_u32(milliwatts), operation)?;
                }
                self.verify_strix_halo_tdp(state)
            })();
            match result {
                Ok(()) => return Ok(()),
                Err(error) => {
                    last_error = error;
                    if attempt < 2 {
                        // PPD can asynchronously restore its policy for a
                        // short period after ActiveProfile changes.
                        thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
        Err(last_error)
    }

    fn verify_strix_halo_tdp(&self, state: TdpState) -> Result<(), String> {
        poll_smu_power_limits(
            || self.read_strix_halo_power_limits(),
            [state.pl2_sppt, state.fppt, state.pl1_spl, state.apu_sppt],
            STRIX_HALO_POWER_LIMIT_SETTLE_TIMEOUT,
            STRIX_HALO_POWER_LIMIT_SETTLE_INTERVAL,
        )
    }

    fn read_strix_halo_power_limits(&self) -> Result<[f32; 4], String> {
        let table = fs::read(self.smu_path("pm_table"))
            .map_err(|error| format!("read SMU PM table: {error}"))?;
        Ok([
            decode_smu_f32(&table, STRIX_HALO_STAPM_PM_TABLE_OFFSET, "STAPM limit")?,
            decode_smu_f32(
                &table,
                STRIX_HALO_FAST_LIMIT_PM_TABLE_OFFSET,
                "PPT fast limit",
            )?,
            decode_smu_f32(
                &table,
                STRIX_HALO_SLOW_LIMIT_PM_TABLE_OFFSET,
                "PPT slow limit",
            )?,
            decode_smu_f32(
                &table,
                STRIX_HALO_APU_SLOW_LIMIT_PM_TABLE_OFFSET,
                "APU PPT limit",
            )?,
        ])
    }
}

fn poll_smu_power_limits(
    mut read_actual: impl FnMut() -> Result<[f32; 4], String>,
    expected: [i32; 4],
    timeout: Duration,
    interval: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let error = match read_actual().and_then(|actual| verify_smu_power_limits(actual, expected))
        {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };

        if Instant::now() >= deadline {
            return Err(error);
        }
        thread::sleep(interval);
    }
}

fn verify_smu_power_limits(actual: [f32; 4], expected: [i32; 4]) -> Result<(), String> {
    let rounded_actual = actual.map(|value| value.round() as i32);
    // STAPM is a firmware-managed skin-temperature limit and may be
    // recalculated independently while the other PPT limits remain fixed.
    if rounded_actual[1..] == expected[1..] {
        Ok(())
    } else {
        Err(format!(
            "SMU power-limit verification failed: requested fast/slow/APU {:?} W, firmware reports {:?} W (STAPM ignored; raw {actual:?})",
            &expected[1..],
            &rounded_actual[1..]
        ))
    }
}

fn battery_health_percent(full: u64, design: u64) -> Option<u8> {
    if design == 0 {
        return None;
    }
    let rounded = full.saturating_mul(100).saturating_add(design / 2) / design;
    Some(rounded.min(100) as u8)
}

fn encode_smu_co(offset: i32) -> Result<[u8; 24], String> {
    if !(-40..=0).contains(&offset) {
        return Err("Curve Optimizer offset must be between -40 and 0".into());
    }
    let encoded = 0x10_0000u32.saturating_sub(offset.unsigned_abs());
    let mut args = [0u8; 24];
    args[..4].copy_from_slice(&encoded.to_le_bytes());
    Ok(args)
}

fn encode_smu_temp_limit(temperature_c: u8) -> Result<[u8; 24], String> {
    if !(80..=99).contains(&temperature_c) {
        return Err("APU temperature limit must be between 80 and 99°C".into());
    }
    let mut args = [0u8; 24];
    args[..4].copy_from_slice(&u32::from(temperature_c).to_le_bytes());
    Ok(args)
}

fn encode_smu_u32(value: u32) -> [u8; 24] {
    let mut args = [0u8; 24];
    args[..4].copy_from_slice(&value.to_le_bytes());
    args
}

fn decode_smu_f32(table: &[u8], offset: usize, name: &str) -> Result<f32, String> {
    let end = offset + size_of::<f32>();
    let bytes: [u8; 4] = table
        .get(offset..end)
        .ok_or_else(|| format!("SMU PM table is too short to verify the {name}"))?
        .try_into()
        .expect("slice length was checked above");
    let value = f32::from_le_bytes(bytes);
    if !value.is_finite() || !(0.0..=255.0).contains(&value) {
        return Err(format!("SMU PM table returned invalid {name} {value}"));
    }
    Ok(value)
}

fn decode_strix_halo_tctl(table: &[u8]) -> Result<u8, String> {
    let end = STRIX_HALO_TCTL_PM_TABLE_OFFSET + size_of::<f32>();
    let bytes: [u8; 4] = table
        .get(STRIX_HALO_TCTL_PM_TABLE_OFFSET..end)
        .ok_or_else(|| "SMU PM table is too short to verify the APU temperature limit".to_string())?
        .try_into()
        .expect("slice length was checked above");
    let temperature = f32::from_le_bytes(bytes);
    if !temperature.is_finite() || !(0.0..=255.0).contains(&temperature) {
        return Err(format!(
            "SMU PM table returned invalid Tctl limit {temperature}"
        ));
    }
    Ok(temperature.round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "z13helper-sysfs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn reads_five_watt_tdp_without_inventing_a_platform_profile() {
        let root = root();
        let ppt = root.join("devices/platform/asus-nb-wmi");
        fs::create_dir_all(&ppt).unwrap();
        for name in [
            "ppt_pl1_spl",
            "ppt_pl2_sppt",
            "ppt_fppt",
            "ppt_apu_sppt",
            "ppt_platform_sppt",
        ] {
            fs::write(ppt.join(name), "5\n").unwrap();
        }
        assert_eq!(Sysfs::new(&root).read_tdp().unwrap().pl1_spl, 5);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn battery_limit_is_clamped_by_validation() {
        assert!(Sysfs::new(root()).set_battery_limit(39).is_err());
    }

    #[test]
    fn reads_battery_charge_and_status() {
        let root = root();
        let battery = root.join("class/power_supply/BAT0");
        fs::create_dir_all(&battery).unwrap();
        fs::write(battery.join("capacity"), "72\n").unwrap();
        fs::write(battery.join("status"), "Discharging\n").unwrap();
        fs::write(battery.join("power_now"), "14500000\n").unwrap();
        fs::write(battery.join("energy_full"), "56000000\n").unwrap();
        fs::write(battery.join("energy_full_design"), "70000000\n").unwrap();

        assert_eq!(
            Sysfs::new(&root).battery_telemetry().unwrap(),
            BatteryTelemetry {
                charge_percent: Some(72),
                status: Some("Discharging".into()),
                power_microwatts: Some(14_500_000),
                health_percent: Some(80),
            }
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reads_power_source_from_adapter_and_falls_back_to_battery_status() {
        let source_root = root();
        let adapter = source_root.join("class/power_supply/AC0");
        fs::create_dir_all(&adapter).unwrap();
        fs::write(adapter.join("type"), "Mains\n").unwrap();
        fs::write(adapter.join("online"), "1\n").unwrap();
        assert!(!Sysfs::new(&source_root).on_battery().unwrap());
        fs::write(adapter.join("online"), "0\n").unwrap();
        assert!(Sysfs::new(&source_root).on_battery().unwrap());
        let _ = fs::remove_dir_all(&source_root);

        let source_root = root();
        let battery = source_root.join("class/power_supply/BAT0");
        fs::create_dir_all(&battery).unwrap();
        fs::write(battery.join("type"), "Battery\n").unwrap();
        fs::write(battery.join("status"), "Charging\n").unwrap();
        assert!(!Sysfs::new(&source_root).on_battery().unwrap());
        fs::write(battery.join("status"), "Discharging\n").unwrap();
        assert!(Sysfs::new(&source_root).on_battery().unwrap());
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    fn battery_health_handles_rounding_bounds_and_missing_design_capacity() {
        assert_eq!(battery_health_percent(63, 70), Some(90));
        assert_eq!(battery_health_percent(71, 70), Some(100));
        assert_eq!(battery_health_percent(70, 0), None);
    }

    #[test]
    fn firmware_curves_write_and_enable_both_fans() {
        let root = root();
        let curve = root.join("class/hwmon/hwmon0");
        let readings = root.join("class/hwmon/hwmon1");
        fs::create_dir_all(&curve).unwrap();
        fs::create_dir_all(&readings).unwrap();
        fs::write(curve.join("name"), "asus_custom_fan_curve\n").unwrap();
        fs::write(readings.join("name"), "asus\n").unwrap();
        for index in 1..=2 {
            fs::write(readings.join(format!("pwm{index}_enable")), "2\n").unwrap();
        }
        let curves = z13helper_core::stock_fan_curves(Some("performance"));
        Sysfs::new(&root).set_firmware_curves(&curves).unwrap();
        assert_eq!(
            fs::read_to_string(curve.join("pwm1_auto_point1_temp")).unwrap(),
            "30\n"
        );
        assert_eq!(
            fs::read_to_string(curve.join("pwm2_auto_point8_pwm")).unwrap(),
            "242\n"
        );
        assert_eq!(
            fs::read_to_string(curve.join("pwm1_enable")).unwrap(),
            "1\n"
        );
        // The generic `asus` hwmon endpoint is for coarse fan control and
        // tachometer readings. Custom-curve writes must not change it.
        assert_eq!(
            fs::read_to_string(readings.join("pwm2_enable")).unwrap(),
            "2\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn factory_curves_request_firmware_reload_and_read_distinct_fans() {
        let root = root();
        let curve_dir = root.join("class/hwmon/hwmon0");
        fs::create_dir_all(&curve_dir).unwrap();
        fs::write(curve_dir.join("name"), "asus_custom_fan_curve\n").unwrap();
        let expected = [
            [
                [50, 2],
                [54, 2],
                [58, 22],
                [62, 30],
                [64, 43],
                [67, 56],
                [71, 68],
                [71, 68],
            ],
            [
                [48, 2],
                [53, 22],
                [57, 33],
                [60, 45],
                [63, 58],
                [65, 71],
                [70, 94],
                [76, 107],
            ],
        ];
        for (fan, curve) in expected.iter().enumerate() {
            let index = fan + 1;
            fs::write(curve_dir.join(format!("pwm{index}_enable")), "2\n").unwrap();
            for (point, [temp, pwm]) in curve.iter().enumerate() {
                let point = point + 1;
                fs::write(
                    curve_dir.join(format!("pwm{index}_auto_point{point}_temp")),
                    format!("{temp}\n"),
                )
                .unwrap();
                fs::write(
                    curve_dir.join(format!("pwm{index}_auto_point{point}_pwm")),
                    format!("{pwm}\n"),
                )
                .unwrap();
            }
        }

        assert_eq!(Sysfs::new(&root).factory_fan_curves().unwrap(), expected);
        assert_eq!(
            fs::read_to_string(curve_dir.join("pwm1_enable")).unwrap(),
            "3\n"
        );
        assert_eq!(
            fs::read_to_string(curve_dir.join("pwm2_enable")).unwrap(),
            "3\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn firmware_release_disables_both_curves_without_generic_fan_writes() {
        let root = root();
        let curve = root.join("class/hwmon/hwmon0");
        let readings = root.join("class/hwmon/hwmon1");
        fs::create_dir_all(&curve).unwrap();
        fs::create_dir_all(&readings).unwrap();
        fs::write(curve.join("name"), "asus_custom_fan_curve\n").unwrap();
        fs::write(readings.join("name"), "asus\n").unwrap();
        for index in 1..=2 {
            fs::write(curve.join(format!("pwm{index}_enable")), "1\n").unwrap();
        }
        fs::write(readings.join("pwm1_enable"), "2\n").unwrap();
        fs::write(readings.join("pwm2_enable"), "0\n").unwrap();

        Sysfs::new(&root).release_firmware_fans().unwrap();

        assert_eq!(
            fs::read_to_string(curve.join("pwm1_enable")).unwrap(),
            "2\n"
        );
        assert_eq!(
            fs::read_to_string(curve.join("pwm2_enable")).unwrap(),
            "2\n"
        );
        assert_eq!(
            fs::read_to_string(readings.join("pwm1_enable")).unwrap(),
            "2\n"
        );
        assert_eq!(
            fs::read_to_string(readings.join("pwm2_enable")).unwrap(),
            "0\n"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn smu_curve_optimizer_bytes_use_mp1_encoding() {
        let bytes = encode_smu_co(-20).unwrap();
        assert_eq!(&bytes[..4], &0x000F_FFECu32.to_le_bytes());
        assert!(bytes[4..].iter().all(|byte| *byte == 0));
        assert!(encode_smu_co(-41).is_err());
    }

    #[test]
    fn smu_temperature_limit_uses_celsius_as_first_argument() {
        let bytes = encode_smu_temp_limit(88).unwrap();
        assert_eq!(&bytes[..4], &88u32.to_le_bytes());
        assert!(bytes[4..].iter().all(|byte| *byte == 0));
        assert!(encode_smu_temp_limit(79).is_err());
        assert!(encode_smu_temp_limit(100).is_err());
    }

    #[test]
    fn strix_halo_tctl_is_read_from_pm_table_index_22() {
        let mut table = vec![0u8; 4096];
        table[STRIX_HALO_TCTL_PM_TABLE_OFFSET..STRIX_HALO_TCTL_PM_TABLE_OFFSET + 4]
            .copy_from_slice(&99.0f32.to_le_bytes());
        assert_eq!(decode_strix_halo_tctl(&table).unwrap(), 99);
        assert!(decode_strix_halo_tctl(&table[..88]).is_err());
    }

    #[test]
    fn smu_power_limit_verification_ignores_float_rounding_noise() {
        assert!(
            verify_smu_power_limits(
                [120.00001, 120.00001, 93.00001, 92.99999],
                [120, 120, 93, 93],
            )
            .is_ok()
        );
    }

    #[test]
    fn smu_power_limit_verification_ignores_firmware_managed_stapm() {
        assert!(verify_smu_power_limits([84.0, 86.0, 70.0, 70.0], [86, 86, 70, 70],).is_ok());
    }

    #[test]
    fn smu_power_limit_verification_waits_through_a_transient_readback() {
        let mut readings = vec![
            [84.00001, 86.00001, 70.00001, 70.00001],
            [86.00001, 86.00001, 70.00001, 70.00001],
        ]
        .into_iter();
        assert!(
            poll_smu_power_limits(
                || Ok(readings.next().expect("test readback")),
                [86, 86, 70, 70],
                Duration::from_millis(10),
                Duration::ZERO,
            )
            .is_ok()
        );
    }

    #[test]
    fn smu_power_limit_verification_rejects_a_real_mismatch() {
        let error =
            verify_smu_power_limits([120.0, 120.0, 94.0, 93.0], [93, 120, 93, 93]).unwrap_err();
        assert!(error.contains("requested fast/slow/APU [120, 93, 93]"));
        assert!(error.contains("firmware reports [120, 94, 93]"));
        assert!(error.contains("STAPM ignored"));
    }
}
