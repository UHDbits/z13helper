use std::fs;
use std::path::{Path, PathBuf};

use z13helper_core::curve::{validate, Curve};
use z13helper_core::protocol::{BatteryTelemetry, TdpState};

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
        fs::write(self.smu_path("smu_args"), args)
            .map_err(|error| format!("write smu_args: {error}"))?;
        fs::write(self.smu_path("mp1_smu_cmd"), 0x4Cu32.to_le_bytes())
            .map_err(|error| format!("write mp1_smu_cmd: {error}"))?;
        let response = fs::read(self.smu_path("mp1_smu_cmd"))
            .map_err(|error| format!("read mp1_smu_cmd: {error}"))?;
        if response.len() < 4 || u32::from_le_bytes(response[..4].try_into().unwrap()) != 1 {
            return Err("SMU rejected Curve Optimizer command".into());
        }
        Ok(())
    }

    pub fn probe_undervolt_once(&self) -> bool {
        self.smu_path("mp1_smu_cmd").exists() && self.send_smu_co(0).is_ok()
    }

    pub fn set_undervolt(&self, offset: i32) -> Result<(), String> {
        self.send_smu_co(offset)
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
}
