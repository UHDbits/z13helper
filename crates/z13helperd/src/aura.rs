use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use z13helper_core::protocol::LightingState;

const KEYBOARD_ID: &str = "0003:00000B05:00001A30";
const LIGHTBAR_ID: &str = "0003:00000B05:000018C6";
const REPORT_ID: u8 = 0x5d;

pub struct AuraDevices {
    sys_root: PathBuf,
    dev_root: PathBuf,
    devices: HashMap<String, (PathBuf, File)>,
}

impl Default for AuraDevices {
    fn default() -> Self {
        Self::new("/sys", "/dev")
    }
}

impl AuraDevices {
    pub fn new(sys_root: impl Into<PathBuf>, dev_root: impl Into<PathBuf>) -> Self {
        Self {
            sys_root: sys_root.into(),
            dev_root: dev_root.into(),
            devices: HashMap::new(),
        }
    }

    pub fn refresh(&mut self) -> Vec<String> {
        let previous = self
            .devices
            .iter()
            .map(|(name, (path, _))| (name.clone(), path.clone()))
            .collect::<HashMap<_, _>>();
        let class = self.sys_root.join("class/hidraw");
        let mut found = HashMap::new();
        for entry in fs::read_dir(class).into_iter().flatten().flatten() {
            let uevent = fs::read_to_string(entry.path().join("device/uevent")).unwrap_or_default();
            let name = if uevent.contains(KEYBOARD_ID) {
                Some("keyboard")
            } else if uevent.contains(LIGHTBAR_ID) {
                Some("lightbar")
            } else {
                None
            };
            if let Some(name) = name {
                let descriptor = entry.path().join("device/report_descriptor");
                if has_aura_report(&descriptor) {
                    found
                        .entry(name.to_owned())
                        .or_insert_with(|| self.dev_root.join(entry.file_name()));
                }
            }
        }
        self.devices.retain(|name, _| found.contains_key(name));
        for (name, path) in found {
            let changed = self
                .devices
                .get(&name)
                .is_none_or(|(previous_path, _)| previous_path != &path);
            if changed {
                self.devices.remove(&name);
                if let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) {
                    self.devices.insert(name, (path, file));
                }
            }
        }
        self.devices
            .keys()
            .filter(|name| previous.get(*name) != self.devices.get(*name).map(|(path, _)| path))
            .cloned()
            .collect()
    }

    pub fn available(&self, device: &str) -> bool {
        self.devices.contains_key(device)
    }

    pub fn apply(&mut self, device: &str, state: &LightingState) -> Result<(), String> {
        let _ = self.refresh();
        let file = self
            .devices
            .get_mut(device)
            .ok_or_else(|| format!("Aura {device} device is not connected"))?;
        apply_to_writer(&mut file.1, state)
    }
}

fn has_aura_report(path: &std::path::Path) -> bool {
    fs::read(path).is_ok_and(|descriptor| descriptor_has_aura_report(&descriptor))
}

fn descriptor_has_aura_report(descriptor: &[u8]) -> bool {
    descriptor.windows(2).any(|item| item == [0x85, REPORT_ID])
}

fn apply_to_writer(writer: &mut impl Write, state: &LightingState) -> Result<(), String> {
    write_report(writer, &[REPORT_ID, 0xB9])?;
    write_report(writer, b"]ASUS Tech.Inc.")?;
    write_report(writer, &[REPORT_ID, 0x05, 0x20, 0x31, 0x00, 0x1A])?;
    write_report(writer, &[REPORT_ID, 0xC0, 0x03, 0x01])?;
    if !state.enabled {
        write_report(writer, &[REPORT_ID, 0xBD, 0x01, 0, 0, 0, 0, 0xFF])?;
        return write_report(writer, &[REPORT_ID, 0xBA, 0xC5, 0xC4, 0]);
    }
    write_report(
        writer,
        &[REPORT_ID, 0xBD, 0x01, 0xFF, 0x1F, 0xFF, 0xFF, 0xFF],
    )?;
    write_report(
        writer,
        &[
            REPORT_ID,
            0xBA,
            0xC5,
            0xC4,
            state.brightness.clamp(0, 3) as u8,
        ],
    )?;
    let [red, green, blue] = parse_color(&state.color)?;
    let [red2, green2, blue2] = parse_color(&state.color2)?;
    let mode = mode_byte(&state.mode)?;
    let random = if [red, green, blue] == [0, 0, 0] {
        0xFF
    } else if mode == 0x01 {
        0x01
    } else {
        0
    };
    // The Z13 protocol requires both zone packets for every physical Aura
    // device. Each device consumes its own zone and ignores the other one.
    for zone in [0, 1] {
        write_report(
            writer,
            &[
                REPORT_ID,
                0xB3,
                zone,
                mode,
                red,
                green,
                blue,
                speed_byte(&state.speed)?,
                0,
                random,
                red2,
                green2,
                blue2,
            ],
        )?;
        write_report(writer, &[REPORT_ID, 0xB5, 0, 0, 0])?;
        write_report(writer, &[REPORT_ID, 0xB4])?;
    }
    Ok(())
}

fn write_report(writer: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    let mut report = [0u8; 64];
    let length = bytes.len().min(report.len());
    report[..length].copy_from_slice(&bytes[..length]);
    writer
        .write_all(&report)
        .map_err(|error| format!("write Aura report: {error}"))
}

fn parse_color(value: &str) -> Result<[u8; 3], String> {
    let value = value.trim_start_matches('#');
    if value.len() != 6 {
        return Err("Aura colors must contain six hexadecimal digits".into());
    }
    let number = u32::from_str_radix(value, 16).map_err(|_| "invalid Aura color".to_owned())?;
    Ok([
        ((number >> 16) & 0xff) as u8,
        ((number >> 8) & 0xff) as u8,
        (number & 0xff) as u8,
    ])
}

fn mode_byte(mode: &str) -> Result<u8, String> {
    match mode {
        "static" => Ok(0x00),
        "breathe" | "breathing" => Ok(0x01),
        "cycle" | "color-cycle" => Ok(0x02),
        "rainbow" => Ok(0x03),
        "strobe" => Ok(0x0A),
        _ => Err(format!("unsupported Aura mode {mode:?}")),
    }
}

fn speed_byte(speed: &str) -> Result<u8, String> {
    match speed {
        "slow" => Ok(0xE1),
        "normal" | "medium" | "" => Ok(0xEB),
        "fast" => Ok(0xF5),
        _ => Err(format!("unsupported Aura speed {speed:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_and_modes_validate() {
        assert_eq!(parse_color("FF8000").unwrap(), [255, 128, 0]);
        assert!(parse_color("black").is_err());
        assert_eq!(mode_byte("rainbow").unwrap(), 3);
        assert_eq!(speed_byte("fast").unwrap(), 0xF5);
    }

    #[test]
    fn only_aura_report_descriptors_are_accepted() {
        assert!(descriptor_has_aura_report(&[0x05, 0x0C, 0x85, 0x5D, 0x09]));
        assert!(!descriptor_has_aura_report(&[0x05, 0x0C, 0x85, 0x5A, 0x09]));
        assert!(!descriptor_has_aura_report(&[0x5D, 0x85]));
    }

    #[test]
    fn apply_writes_selected_color_to_both_z13_zones() {
        let mut reports = Vec::new();
        let state = LightingState {
            enabled: true,
            mode: "static".into(),
            color: "F6D32D".into(),
            color2: "000000".into(),
            speed: "normal".into(),
            brightness: 3,
        };

        apply_to_writer(&mut reports, &state).unwrap();

        let mode_reports = reports
            .chunks_exact(64)
            .filter(|report| report[..2] == [REPORT_ID, 0xB3])
            .collect::<Vec<_>>();
        assert_eq!(mode_reports.len(), 2);
        assert_eq!(mode_reports[0][2..8], [0, 0, 0xF6, 0xD3, 0x2D, 0xEB]);
        assert_eq!(mode_reports[1][2..8], [1, 0, 0xF6, 0xD3, 0x2D, 0xEB]);
    }

    #[test]
    fn refresh_reopens_aura_handles_when_hidraw_path_changes() {
        let root = std::env::temp_dir().join(format!(
            "z13helper-aura-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sys_root = root.join("sys");
        let dev_root = root.join("dev");
        let make_device = |name: &str| {
            let device = sys_root.join("class/hidraw").join(name).join("device");
            fs::create_dir_all(&device).unwrap();
            fs::write(device.join("uevent"), KEYBOARD_ID).unwrap();
            fs::write(device.join("report_descriptor"), [0x85, REPORT_ID]).unwrap();
            fs::create_dir_all(&dev_root).unwrap();
            fs::write(dev_root.join(name), []).unwrap();
        };

        make_device("hidraw0");
        let mut devices = AuraDevices::new(&sys_root, &dev_root);
        assert_eq!(devices.refresh(), ["keyboard"]);
        fs::remove_dir_all(sys_root.join("class/hidraw/hidraw0")).unwrap();
        fs::remove_file(dev_root.join("hidraw0")).unwrap();
        make_device("hidraw1");
        assert_eq!(devices.refresh(), ["keyboard"]);
        assert_eq!(devices.devices["keyboard"].0, dev_root.join("hidraw1"));
        fs::remove_dir_all(root).unwrap();
    }
}
