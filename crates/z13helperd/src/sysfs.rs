use std::fs;
use std::path::{Path, PathBuf};

use z13helper_core::curve::{validate, Curve};
use z13helper_core::profile::Base;
use z13helper_core::protocol::TdpState;

const STOCK_QUIET: TdpState = TdpState {
    pl1_spl: 40,
    pl2_sppt: 55,
    fppt: 55,
    apu_sppt: 70,
    platform_sppt: 70,
};
const STOCK_BALANCED: TdpState = TdpState {
    pl1_spl: 52,
    pl2_sppt: 71,
    fppt: 70,
    apu_sppt: 70,
    platform_sppt: 70,
};
const STOCK_PERFORMANCE: TdpState = TdpState {
    pl1_spl: 70,
    pl2_sppt: 86,
    fppt: 86,
    apu_sppt: 70,
    platform_sppt: 70,
};

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

    fn profile_devices(&self) -> Vec<PathBuf> {
        let directory = self.path("/sys/class/platform-profile");
        fs::read_dir(directory)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.join("profile").exists())
            .collect()
    }

    fn supports_profile(&self, directory: &Path, name: &str) -> bool {
        self.read_text(directory.join("choices"))
            .map(|choices| choices.split_whitespace().any(|choice| choice == name))
            .unwrap_or(false)
    }

    pub fn read_base(&self) -> Result<Base, String> {
        let preferred = self
            .profile_devices()
            .into_iter()
            .find(|path| self.supports_profile(path, "quiet"))
            .map(|path| path.join("profile"))
            .unwrap_or_else(|| self.path("/sys/firmware/acpi/platform_profile"));
        let value = self.read_text(preferred)?;
        Base::from_str_lossy(&value)
            .ok_or_else(|| format!("unsupported platform profile {value:?}"))
    }

    pub fn set_base(&self, base: Base) -> Result<(), String> {
        let devices = self.profile_devices();
        if devices.is_empty() {
            return self.write_text(
                self.path("/sys/firmware/acpi/platform_profile"),
                base.as_str(),
            );
        }
        for directory in &devices {
            let name = if base == Base::Quiet
                && !self.supports_profile(directory, "quiet")
                && self.supports_profile(directory, "low-power")
            {
                "low-power"
            } else {
                base.as_str()
            };
            self.write_text(directory.join("profile"), name)?;
        }
        Ok(())
    }

    pub fn stock_tdp(base: Base) -> TdpState {
        match base {
            Base::Quiet => STOCK_QUIET,
            Base::Balanced => STOCK_BALANCED,
            Base::Performance => STOCK_PERFORMANCE,
        }
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

    pub fn read_tdp(&self, base: Base) -> Result<TdpState, String> {
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
        Ok(if state.pl1_spl == 5 {
            Self::stock_tdp(base)
        } else {
            state
        })
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
    fn stale_five_watt_read_uses_stock_table() {
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
        assert_eq!(
            Sysfs::new(&root).read_tdp(Base::Balanced).unwrap(),
            STOCK_BALANCED
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn battery_limit_is_clamped_by_validation() {
        assert!(Sysfs::new(root()).set_battery_limit(39).is_err());
    }

    #[test]
    fn writes_every_platform_profile_device_with_quiet_mapping() {
        let root = root();
        let profiles = root.join("class/platform-profile");
        for (device, choices) in [
            ("cpu", "quiet balanced performance"),
            ("gpu", "low-power balanced performance"),
        ] {
            let directory = profiles.join(device);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("choices"), choices).unwrap();
            fs::write(directory.join("profile"), "balanced").unwrap();
        }
        Sysfs::new(&root).set_base(Base::Quiet).unwrap();
        assert_eq!(
            fs::read_to_string(profiles.join("cpu/profile")).unwrap(),
            "quiet\n"
        );
        assert_eq!(
            fs::read_to_string(profiles.join("gpu/profile")).unwrap(),
            "low-power\n"
        );
        let _ = fs::remove_dir_all(root);
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
        let curves = [
            Base::Quiet.stock_fan_curve(),
            Base::Performance.stock_fan_curve(),
        ];
        Sysfs::new(&root).set_firmware_curves(&curves).unwrap();
        assert_eq!(
            fs::read_to_string(curve.join("pwm1_auto_point1_temp")).unwrap(),
            "30\n"
        );
        assert_eq!(
            fs::read_to_string(curve.join("pwm2_auto_point8_pwm")).unwrap(),
            "229\n"
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
