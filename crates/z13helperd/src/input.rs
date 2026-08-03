use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};
use z13helper_core::{ControllerAction, DaemonEventKind};

const KEY_PROG3: u16 = 202;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const ABS_HAT0X: u16 = 16;
const ABS_HAT0Y: u16 = 17;
const INPUT_PROP_ACCELEROMETER: usize = 6;
const BTN_SOUTH: u16 = 304;
const BTN_EAST: u16 = 305;
const BTN_DPAD_UP: u16 = 544;
const BTN_DPAD_DOWN: u16 = 545;
const BTN_DPAD_LEFT: u16 = 546;
const BTN_DPAD_RIGHT: u16 = 547;
const STEAM_VENDOR: u16 = 0x28de;
const STEAM_VIRTUAL_GAMEPAD: u16 = 0x11ff;
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;
const SCAN_INTERVAL: Duration = Duration::from_secs(2);
const REPEAT_INITIAL: Duration = Duration::from_millis(400);
const REPEAT_INTERVAL: Duration = Duration::from_millis(120);
/// ~40% of signed 16-bit stick range; engage a digital direction.
const STICK_ENGAGE: i32 = 13107;
/// ~25% of signed 16-bit stick range; release with hysteresis below engage.
const STICK_RELEASE: i32 = 8192;

const GAMEPAD_BUTTONS: &[usize] = &[
    304, 305, 307, 308, 310, 311, 312, 313, 314, 315, 316, 317, 318, 319,
];

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeviceClass {
    Ignore,
    Controller,
    GrabOnly,
}

#[derive(Default)]
struct CapabilityBits(Vec<u64>);

impl CapabilityBits {
    fn parse(text: &str) -> Self {
        Self(
            text.split_whitespace()
                .rev()
                .filter_map(|word| u64::from_str_radix(word, 16).ok())
                .collect(),
        )
    }

    fn contains(&self, bit: usize) -> bool {
        self.0
            .get(bit / 64)
            .is_some_and(|word| word & (1_u64 << (bit % 64)) != 0)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct StickState {
    x: i32,
    y: i32,
    direction: Option<ControllerAction>,
}

struct ControllerDevice {
    path: PathBuf,
    file: File,
    class: DeviceClass,
    grabbed: bool,
    held: HashMap<ControllerAction, Instant>,
    stick: StickState,
}

pub struct ControllerCaptureHandle {
    control: UnixDatagram,
}

impl ControllerCaptureHandle {
    pub fn set_enabled(&self, enabled: bool) -> io::Result<()> {
        self.control.send(&[u8::from(enabled)]).map(|_| ())
    }
}

/// Scan and read controllers in the privileged daemon. The GTK process never
/// opens `/dev/input`; it receives only normalized actions over the socket.
/// `capture` is a short-lived lease maintained by the GUI, so a crashed GUI
/// cannot leave controllers exclusively grabbed from games.
pub fn spawn_controller_watcher(
    sender: Sender<ControllerAction>,
    terminate: Arc<AtomicBool>,
) -> io::Result<ControllerCaptureHandle> {
    let (control, receiver) = UnixDatagram::pair()?;
    control.set_nonblocking(true)?;
    receiver.set_nonblocking(true)?;
    std::thread::spawn(move || controller_loop(sender, receiver, terminate));
    Ok(ControllerCaptureHandle { control })
}

fn controller_loop(
    sender: Sender<ControllerAction>,
    control: UnixDatagram,
    terminate: Arc<AtomicBool>,
) {
    let mut devices = Vec::<ControllerDevice>::new();
    let mut last_scan = Instant::now() - SCAN_INTERVAL;
    let mut capturing = false;

    while !terminate.load(Ordering::Relaxed) {
        if last_scan.elapsed() >= SCAN_INTERVAL {
            scan_controllers(&mut devices, capturing);
            last_scan = Instant::now();
        }

        let mut poll_fds = Vec::with_capacity(devices.len() + 1);
        poll_fds.push(libc::pollfd {
            fd: control.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        poll_fds.extend(devices.iter().map(|device| libc::pollfd {
            fd: device.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }));
        let timeout = next_poll_timeout(last_scan, capturing, &devices);
        // SAFETY: `poll_fds` owns a stable contiguous allocation for the call,
        // and every fd remains open until poll returns.
        let result = unsafe {
            libc::poll(
                poll_fds.as_mut_ptr(),
                poll_fds.len() as libc::nfds_t,
                timeout,
            )
        };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                tracing::warn!(%error, "controller poll failed");
                std::thread::sleep(Duration::from_millis(100));
            }
            continue;
        }

        if poll_fds[0].revents & libc::POLLIN != 0
            && let Some(requested) = receive_capture_state(&control)
            && requested != capturing
        {
            for device in &mut devices {
                if requested {
                    drain_device(device);
                }
                set_grabbed(device, requested);
                device.held.clear();
                device.stick = StickState::default();
            }
            capturing = requested;
        }

        let ready: HashSet<_> = poll_fds
            .iter()
            .skip(1)
            .filter(|poll| poll.revents != 0)
            .map(|poll| poll.fd)
            .collect();
        devices.retain_mut(|device| {
            if !ready.contains(&device.file.as_raw_fd()) {
                return true;
            }
            match read_device(device, capturing, &sender) {
                Ok(()) => true,
                Err(error) => {
                    tracing::info!(
                        path = %device.path.display(),
                        %error,
                        "controller disconnected"
                    );
                    false
                }
            }
        });
        if capturing {
            emit_repeats(&mut devices, &sender);
        }
    }

    for device in &mut devices {
        set_grabbed(device, false);
    }
}

fn receive_capture_state(control: &UnixDatagram) -> Option<bool> {
    let mut latest = None;
    let mut states = [0_u8; 32];
    loop {
        match control.recv(&mut states) {
            Ok(0) => break,
            Ok(count) => latest = Some(states[count - 1] != 0),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => {
                tracing::warn!(%error, "controller control channel failed");
                break;
            }
        }
    }
    latest
}

fn next_poll_timeout(last_scan: Instant, capturing: bool, devices: &[ControllerDevice]) -> i32 {
    let now = Instant::now();
    let next_scan = last_scan + SCAN_INTERVAL;
    let mut deadline = next_scan;
    if capturing && let Some(repeat) = devices.iter().flat_map(|device| device.held.values()).min()
    {
        deadline = deadline.min(*repeat);
    }
    let remaining = deadline.saturating_duration_since(now);
    remaining.as_millis().max(1).min(i32::MAX as u128) as i32
}

fn scan_controllers(devices: &mut Vec<ControllerDevice>, capture: bool) {
    let known: HashSet<PathBuf> = devices.iter().map(|device| device.path.clone()).collect();
    let Ok(entries) = fs::read_dir("/sys/class/input") else {
        return;
    };
    for entry in entries.flatten() {
        let event_name = entry.file_name();
        if !event_name.to_string_lossy().starts_with("event") {
            continue;
        }
        let path = PathBuf::from("/dev/input").join(&event_name);
        if known.contains(&path) {
            continue;
        }
        let class = classify_device(&entry.path());
        if class == DeviceClass::Ignore {
            continue;
        }
        let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        else {
            continue;
        };
        let name = read_trimmed(entry.path().join("device/name"));
        let mut device = ControllerDevice {
            path,
            file,
            class,
            grabbed: false,
            held: HashMap::new(),
            stick: StickState::default(),
        };
        if capture {
            drain_device(&mut device);
            set_grabbed(&mut device, true);
        }
        tracing::info!(
            path = %device.path.display(),
            name,
            class = ?class,
            "controller input device found"
        );
        devices.push(device);
    }
}

fn classify_device(event_sysfs: &std::path::Path) -> DeviceClass {
    let device = event_sysfs.join("device");
    let properties = CapabilityBits::parse(&read_trimmed(device.join("properties")));
    let keys = CapabilityBits::parse(&read_trimmed(device.join("capabilities/key")));
    let vendor = read_hex_u16(device.join("id/vendor"));
    let product = read_hex_u16(device.join("id/product"));
    classify_capabilities(&properties, &keys, vendor, product)
}

fn classify_capabilities(
    properties: &CapabilityBits,
    keys: &CapabilityBits,
    vendor: Option<u16>,
    product: Option<u16>,
) -> DeviceClass {
    if properties.contains(INPUT_PROP_ACCELEROMETER) {
        return DeviceClass::Ignore;
    }
    let has_gamepad_button = GAMEPAD_BUTTONS.iter().any(|button| keys.contains(*button));
    if has_gamepad_button {
        if vendor == Some(STEAM_VENDOR) && product == Some(STEAM_VIRTUAL_GAMEPAD) {
            DeviceClass::GrabOnly
        } else {
            DeviceClass::Controller
        }
    } else {
        DeviceClass::Ignore
    }
}

fn read_trimmed(path: PathBuf) -> String {
    fs::read_to_string(path)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

fn read_hex_u16(path: PathBuf) -> Option<u16> {
    u16::from_str_radix(&read_trimmed(path), 16).ok()
}

fn set_grabbed(device: &mut ControllerDevice, grabbed: bool) {
    if device.grabbed == grabbed {
        return;
    }
    // SAFETY: EVIOCGRAB accepts an integer value and the fd remains owned by
    // `device.file` for the duration of this call.
    let result = unsafe { libc::ioctl(device.file.as_raw_fd(), EVIOCGRAB, i32::from(grabbed)) };
    if result == 0 {
        device.grabbed = grabbed;
        tracing::info!(
            path = %device.path.display(),
            grabbed,
            "controller exclusive capture changed"
        );
    } else {
        tracing::warn!(
            path = %device.path.display(),
            grabbed,
            error = %io::Error::last_os_error(),
            "controller exclusive capture failed"
        );
    }
}

fn drain_device(device: &mut ControllerDevice) {
    let mut bytes = [0_u8; 24 * 32];
    loop {
        match device.file.read(&mut bytes) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}

fn read_device(
    device: &mut ControllerDevice,
    capture: bool,
    sender: &Sender<ControllerAction>,
) -> io::Result<()> {
    let event_size = std::mem::size_of::<libc::timeval>() + 8;
    let mut bytes = [0_u8; 24 * 32];
    loop {
        let count = match device.file.read(&mut bytes) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        };
        if !capture || !device.grabbed || device.class != DeviceClass::Controller {
            continue;
        }
        for event in bytes[..count].chunks_exact(event_size) {
            let offset = std::mem::size_of::<libc::timeval>();
            let event_type = u16::from_ne_bytes(event[offset..offset + 2].try_into().unwrap());
            let code = u16::from_ne_bytes(event[offset + 2..offset + 4].try_into().unwrap());
            let value = i32::from_ne_bytes(event[offset + 4..offset + 8].try_into().unwrap());
            handle_controller_event(device, event_type, code, value, sender);
        }
    }
}

fn handle_controller_event(
    device: &mut ControllerDevice,
    event_type: u16,
    code: u16,
    value: i32,
    sender: &Sender<ControllerAction>,
) {
    match event_type {
        EV_KEY => {
            let action = match code {
                BTN_SOUTH => Some(ControllerAction::Accept),
                BTN_EAST => Some(ControllerAction::Back),
                BTN_DPAD_UP => Some(ControllerAction::Up),
                BTN_DPAD_DOWN => Some(ControllerAction::Down),
                BTN_DPAD_LEFT => Some(ControllerAction::Left),
                BTN_DPAD_RIGHT => Some(ControllerAction::Right),
                _ => None,
            };
            let Some(action) = action else {
                return;
            };
            if value == 1 {
                emit_press(device, action, sender);
            } else if value == 0 {
                device.held.remove(&action);
            }
        }
        EV_ABS if code == ABS_X || code == ABS_Y => {
            if code == ABS_X {
                device.stick.x = value;
            } else {
                device.stick.y = value;
            }
            apply_stick_direction(device, sender);
        }
        EV_ABS if code == ABS_HAT0X => {
            device.held.remove(&ControllerAction::Left);
            device.held.remove(&ControllerAction::Right);
            if value < 0 {
                emit_press(device, ControllerAction::Left, sender);
            } else if value > 0 {
                emit_press(device, ControllerAction::Right, sender);
            }
        }
        EV_ABS if code == ABS_HAT0Y => {
            device.held.remove(&ControllerAction::Up);
            device.held.remove(&ControllerAction::Down);
            if value < 0 {
                emit_press(device, ControllerAction::Up, sender);
            } else if value > 0 {
                emit_press(device, ControllerAction::Down, sender);
            }
        }
        _ => {}
    }
}

/// Map left-stick axes to a single digital direction with deadzone hysteresis.
/// Dominant-axis selection prevents diagonals from alternating Left/Up.
fn stick_direction(x: i32, y: i32, current: Option<ControllerAction>) -> Option<ControllerAction> {
    let ax = x.saturating_abs();
    let ay = y.saturating_abs();
    let dominant = |horizontal: bool| -> Option<ControllerAction> {
        if horizontal {
            if x < 0 {
                Some(ControllerAction::Left)
            } else if x > 0 {
                Some(ControllerAction::Right)
            } else {
                None
            }
        } else if y < 0 {
            Some(ControllerAction::Up)
        } else if y > 0 {
            Some(ControllerAction::Down)
        } else {
            None
        }
    };

    match current {
        None => {
            if ax < STICK_ENGAGE && ay < STICK_ENGAGE {
                None
            } else if ax >= ay {
                dominant(true)
            } else {
                dominant(false)
            }
        }
        Some(held) => {
            let held_horizontal = matches!(held, ControllerAction::Left | ControllerAction::Right);
            let other_dominates = if held_horizontal {
                ay > ax && ay >= STICK_ENGAGE
            } else {
                ax > ay && ax >= STICK_ENGAGE
            };
            if other_dominates {
                return if ax >= ay {
                    dominant(true)
                } else {
                    dominant(false)
                };
            }

            let held_aligned = match held {
                ControllerAction::Left => x < 0,
                ControllerAction::Right => x > 0,
                ControllerAction::Up => y < 0,
                ControllerAction::Down => y > 0,
                ControllerAction::Accept | ControllerAction::Back => false,
            };
            let held_mag = if held_horizontal { ax } else { ay };
            if held_aligned && held_mag >= STICK_RELEASE {
                Some(held)
            } else if ax >= STICK_ENGAGE || ay >= STICK_ENGAGE {
                if ax >= ay {
                    dominant(true)
                } else {
                    dominant(false)
                }
            } else {
                None
            }
        }
    }
}

fn apply_stick_direction(device: &mut ControllerDevice, sender: &Sender<ControllerAction>) {
    let next = stick_direction(device.stick.x, device.stick.y, device.stick.direction);
    if next == device.stick.direction {
        return;
    }
    if let Some(previous) = device.stick.direction {
        device.held.remove(&previous);
    }
    device.stick.direction = next;
    if let Some(action) = next {
        emit_press(device, action, sender);
    }
}

fn emit_press(
    device: &mut ControllerDevice,
    action: ControllerAction,
    sender: &Sender<ControllerAction>,
) {
    let _ = sender.send(action);
    if matches!(
        action,
        ControllerAction::Up
            | ControllerAction::Down
            | ControllerAction::Left
            | ControllerAction::Right
    ) {
        device.held.insert(action, Instant::now() + REPEAT_INITIAL);
    }
}

fn emit_repeats(devices: &mut [ControllerDevice], sender: &Sender<ControllerAction>) {
    let now = Instant::now();
    for device in devices {
        if !device.grabbed || device.class != DeviceClass::Controller {
            continue;
        }
        for (action, deadline) in &mut device.held {
            if now >= *deadline {
                let _ = sender.send(*action);
                *deadline = now + REPEAT_INTERVAL;
            }
        }
    }
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
        assert_eq!(ABS_X, 0);
        assert_eq!(ABS_Y, 1);
        assert_eq!(BTN_SOUTH, 0x130);
        assert_eq!(BTN_EAST, 0x131);
    }

    #[test]
    fn stick_engages_above_deadzone_and_releases_with_hysteresis() {
        assert_eq!(stick_direction(0, 0, None), None);
        assert_eq!(stick_direction(STICK_ENGAGE - 1, 0, None), None);
        assert_eq!(
            stick_direction(STICK_ENGAGE, 0, None),
            Some(ControllerAction::Right)
        );
        assert_eq!(
            stick_direction(0, -STICK_ENGAGE, None),
            Some(ControllerAction::Up)
        );
        assert_eq!(
            stick_direction(STICK_RELEASE, 0, Some(ControllerAction::Right)),
            Some(ControllerAction::Right)
        );
        assert_eq!(
            stick_direction(STICK_RELEASE - 1, 0, Some(ControllerAction::Right)),
            None
        );
    }

    #[test]
    fn stick_picks_dominant_axis_on_diagonals() {
        assert_eq!(
            stick_direction(20_000, 10_000, None),
            Some(ControllerAction::Right)
        );
        assert_eq!(
            stick_direction(10_000, -20_000, None),
            Some(ControllerAction::Up)
        );
        assert_eq!(
            stick_direction(-20_000, 20_000, None),
            Some(ControllerAction::Left)
        );
    }

    #[test]
    fn stick_can_switch_axes_while_held() {
        assert_eq!(
            stick_direction(5_000, -20_000, Some(ControllerAction::Right)),
            Some(ControllerAction::Up)
        );
        assert_eq!(
            stick_direction(-20_000, 5_000, Some(ControllerAction::Down)),
            Some(ControllerAction::Left)
        );
    }

    #[test]
    fn parses_kernel_capability_bitmaps_from_high_words_first() {
        let bits = CapabilityBits::parse("100000000 00000000 00000001");
        assert!(bits.contains(0));
        assert!(bits.contains(160));
        assert!(!bits.contains(159));
    }

    #[test]
    fn capture_channel_coalesces_to_the_latest_state() {
        let (sender, receiver) = UnixDatagram::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        sender.send(&[1]).unwrap();
        sender.send(&[0]).unwrap();
        assert_eq!(receive_capture_state(&receiver), Some(false));
        assert_eq!(receive_capture_state(&receiver), None);
    }

    #[test]
    fn classifies_controllers_without_grabbing_the_touchscreen() {
        let mut gamepad_keys = CapabilityBits::default();
        gamepad_keys.0.resize(5, 0);
        gamepad_keys.0[BTN_SOUTH as usize / 64] |= 1 << (BTN_SOUTH as usize % 64);
        assert_eq!(
            classify_capabilities(&CapabilityBits::default(), &gamepad_keys, None, None,),
            DeviceClass::Controller
        );

        // Input nodes without actual gamepad buttons must remain available;
        // this includes the detachable keyboard touchpad and the touchscreen.
        assert_eq!(
            classify_capabilities(
                &CapabilityBits::default(),
                &CapabilityBits::default(),
                None,
                None
            ),
            DeviceClass::Ignore
        );
        assert_eq!(
            classify_capabilities(
                &CapabilityBits::default(),
                &gamepad_keys,
                Some(STEAM_VENDOR),
                Some(STEAM_VIRTUAL_GAMEPAD),
            ),
            DeviceClass::GrabOnly
        );
    }
}
