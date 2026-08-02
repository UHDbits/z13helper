use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;
use z13helper_core::DaemonEventKind;

const KEY_PROG3: u16 = 202;
const EV_KEY: u16 = 1;

fn find_button_device() -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/input").ok()?.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("event") {
            continue;
        }
        let name = fs::read_to_string(entry.path().join("device/name")).unwrap_or_default();
        if name.trim() == "Asus WMI hotkeys" {
            return Some(PathBuf::from("/dev/input").join(entry.file_name()));
        }
    }
    None
}

pub fn spawn_button_watcher(sender: Sender<DaemonEventKind>, terminate: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !terminate.load(Ordering::Relaxed) {
            let Some(path) = find_button_device() else {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            };
            let Ok(mut device) = File::open(&path) else {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            };
            if let Err(error) = read_events(&mut device, &sender, &terminate) {
                tracing::warn!(%error, path = %path.display(), "button watcher reconnecting");
            }
        }
    });
}

fn read_events(
    device: &mut File,
    sender: &Sender<DaemonEventKind>,
    terminate: &AtomicBool,
) -> Result<(), String> {
    let event_size = std::mem::size_of::<libc::timeval>() + 8;
    let mut event = vec![0u8; event_size];
    while !terminate.load(Ordering::Relaxed) {
        device
            .read_exact(&mut event)
            .map_err(|error| error.to_string())?;
        let offset = std::mem::size_of::<libc::timeval>();
        let event_type = u16::from_ne_bytes(event[offset..offset + 2].try_into().unwrap());
        let code = u16::from_ne_bytes(event[offset + 2..offset + 4].try_into().unwrap());
        let value = i32::from_ne_bytes(event[offset + 4..offset + 8].try_into().unwrap());
        if event_type == EV_KEY && code == KEY_PROG3 && value == 1 {
            let _ = sender.send(DaemonEventKind::GuiToggle);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_constants_match_linux_input() {
        assert_eq!(EV_KEY, 1);
        assert_eq!(KEY_PROG3, 202);
    }
}
