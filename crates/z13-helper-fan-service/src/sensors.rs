use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

const HWMON_ROOT: &str = "/sys/class/hwmon";
const ASUS_NB_WMI_ROOT: &str = "/sys/devices/platform/asus-nb-wmi";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SensorSnapshot {
    pub apu_temperature_c: i32,
    pub rpm: [u32; 2],
    pub pl1_w: Option<u32>,
}

#[derive(Debug, Error)]
pub enum SensorError {
    #[error("failed to enumerate hwmon: {0}")]
    Enumerate(#[source] io::Error),
    #[error("APU temperature sensor was not found")]
    TemperatureMissing,
    #[error("two fan RPM sensors were not found")]
    FansMissing,
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid numeric sensor value in {0}")]
    Invalid(PathBuf),
    #[error("APU temperature {0}C is outside the credible range")]
    TemperatureRange(i32),
}

pub fn read_snapshot() -> Result<SensorSnapshot, SensorError> {
    let mut snapshot = read_snapshot_from(Path::new(HWMON_ROOT))?;
    if snapshot.pl1_w.is_none() {
        snapshot.pl1_w = read_platform_pl1(Path::new(ASUS_NB_WMI_ROOT))?;
    }
    Ok(snapshot)
}

fn read_trimmed(path: &Path) -> Result<String, SensorError> {
    fs::read_to_string(path)
        .map(|value| value.trim().to_owned())
        .map_err(|source| SensorError::Read {
            path: path.to_owned(),
            source,
        })
}

fn read_i64(path: &Path) -> Result<i64, SensorError> {
    read_trimmed(path)?
        .parse()
        .map_err(|_| SensorError::Invalid(path.to_owned()))
}

fn read_platform_pl1(directory: &Path) -> Result<Option<u32>, SensorError> {
    let path = directory.join("ppt_pl1_spl");
    if !path.exists() {
        return Ok(None);
    }
    let value = read_i64(&path)?;
    if value < 0 {
        return Err(SensorError::Invalid(path));
    }
    u32::try_from(value)
        .map(Some)
        .map_err(|_| SensorError::Invalid(path))
}

fn indexed_inputs(directory: &Path, prefix: &str) -> Result<Vec<PathBuf>, SensorError> {
    let mut paths = Vec::new();
    let entries = fs::read_dir(directory).map_err(SensorError::Enumerate)?;
    for entry in entries {
        let entry = entry.map_err(SensorError::Enumerate)?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(prefix)
            && name.ends_with("_input")
            && name[prefix.len()..name.len() - "_input".len()]
                .chars()
                .all(|character| character.is_ascii_digit())
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

pub fn read_snapshot_from(root: &Path) -> Result<SensorSnapshot, SensorError> {
    let mut entries = fs::read_dir(root)
        .map_err(SensorError::Enumerate)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(SensorError::Enumerate)?;
    entries.sort_by_key(|entry| entry.file_name());

    let mut temperature = None;
    let mut fallback_temperature = None;
    let mut preferred_rpms = Vec::new();
    let mut fallback_rpms = Vec::new();
    let mut pl1_w = None;

    for entry in entries {
        let directory = entry.path();
        let name_path = directory.join("name");
        let Ok(name) = read_trimmed(&name_path) else {
            continue;
        };

        for input in indexed_inputs(&directory, "temp")? {
            let stem = input
                .file_name()
                .and_then(|name| name.to_str())
                .expect("hwmon filename is valid UTF-8")
                .trim_end_matches("_input");
            let label = fs::read_to_string(directory.join(format!("{stem}_label")))
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            if fallback_temperature.is_none() && name == "k10temp" {
                fallback_temperature = Some(input.clone());
            }
            if name == "k10temp" && matches!(label.as_str(), "tctl" | "tdie") {
                temperature = Some(input);
            }
        }

        let fans = indexed_inputs(&directory, "fan")?;
        if name == "asus" {
            preferred_rpms.extend(fans);
        } else {
            fallback_rpms.extend(fans);
        }

        if name == "asus-nb-wmi" {
            for (attribute, divisor) in [("ppt_pl1_spl", 1_i64), ("power1_cap", 1_000_000)] {
                let path = directory.join(attribute);
                if path.exists() {
                    let raw = read_i64(&path)?;
                    if raw >= 0 {
                        pl1_w = u32::try_from(raw / divisor).ok();
                        break;
                    }
                }
            }
        }
    }

    let temperature_path = temperature
        .or(fallback_temperature)
        .ok_or(SensorError::TemperatureMissing)?;
    let temperature_c = i32::try_from(read_i64(&temperature_path)? / 1_000)
        .map_err(|_| SensorError::Invalid(temperature_path.clone()))?;
    if !(-20..=150).contains(&temperature_c) {
        return Err(SensorError::TemperatureRange(temperature_c));
    }

    preferred_rpms.sort();
    fallback_rpms.sort();
    let rpms = if preferred_rpms.len() >= 2 {
        preferred_rpms
    } else {
        fallback_rpms
    };
    if rpms.len() < 2 {
        return Err(SensorError::FansMissing);
    }
    let rpm = [
        u32::try_from(read_i64(&rpms[0])?).map_err(|_| SensorError::Invalid(rpms[0].clone()))?,
        u32::try_from(read_i64(&rpms[1])?).map_err(|_| SensorError::Invalid(rpms[1].clone()))?,
    ];

    Ok(SensorSnapshot {
        apu_temperature_c: temperature_c,
        rpm,
        pl1_w,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "z13-helper-fan-service-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, relative: &str, value: &str) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, value).unwrap();
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn dynamically_discovers_temperature_fans_and_pl1() {
        let tree = TempTree::new();
        tree.write("hwmon0/name", "k10temp\n");
        tree.write("hwmon0/temp1_label", "Tctl\n");
        tree.write("hwmon0/temp1_input", "67500\n");
        tree.write("hwmon1/name", "asus\n");
        tree.write("hwmon1/fan1_input", "3100\n");
        tree.write("hwmon1/fan2_input", "2900\n");
        tree.write("hwmon2/name", "asus-nb-wmi\n");
        tree.write("hwmon2/power1_cap", "80000000\n");

        assert_eq!(
            read_snapshot_from(&tree.0).unwrap(),
            SensorSnapshot {
                apu_temperature_c: 67,
                rpm: [3100, 2900],
                pl1_w: Some(80),
            }
        );
    }

    #[test]
    fn prefers_asus_hwmon_fans_over_other_devices() {
        let tree = TempTree::new();
        tree.write("hwmon0/name", "k10temp\n");
        tree.write("hwmon0/temp1_label", "Tctl\n");
        tree.write("hwmon0/temp1_input", "50000\n");
        tree.write("hwmon1/name", "nvme\n");
        tree.write("hwmon1/fan1_input", "111\n");
        tree.write("hwmon1/fan2_input", "222\n");
        tree.write("hwmon2/name", "asus\n");
        tree.write("hwmon2/fan1_input", "2400\n");
        tree.write("hwmon2/fan2_input", "2800\n");

        assert_eq!(read_snapshot_from(&tree.0).unwrap().rpm, [2400, 2800]);
    }

    #[test]
    fn reads_pl1_from_asus_nb_wmi_platform_attribute() {
        let tree = TempTree::new();
        tree.write("ppt_pl1_spl", "76\n");
        assert_eq!(read_platform_pl1(&tree.0).unwrap(), Some(76));
    }
}
