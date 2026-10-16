use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::time::{Duration, Instant};
use z13helper_core::{ControllerAction, DaemonEventKind};

const KEY_PROG3: u16 = 202;
const EV_SYN: u16 = 0;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
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
// EVIOCGABS(axis) is _IOR('E', 0x40 + axis, struct input_absinfo). The
// struct is six i32 values on every Linux ABI supported by this daemon.
const EVIOCGABS_BASE: libc::c_ulong = 0x8018_4540;
const SCAN_INTERVAL: Duration = Duration::from_secs(2);
const REPEAT_INITIAL: Duration = Duration::from_millis(400);
const REPEAT_INTERVAL: Duration = Duration::from_millis(120);
/// ~40% of signed 16-bit stick range; engage a digital direction.
const STICK_ENGAGE: i32 = 13107;
/// ~25% of signed 16-bit stick range; release with hysteresis below engage.
const STICK_RELEASE: i32 = 8192;
const CAPTURE_COMMAND: u8 = 0x01;
/// Size of `struct input_event`: a timeval, then u16 type, u16 code, i32 value.
const EVENT_BYTES: usize = std::mem::size_of::<libc::timeval>() + 8;

const GAMEPAD_BUTTONS: &[usize] = &[
    304, 305, 307, 308, 310, 311, 312, 313, 314, 315, 316, 317, 318, 319,
];

fn find_button_device() -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/input").ok()?.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("event") {
            continue;
        }
        if read_trimmed(entry.path().join("device/name")) == "Asus WMI hotkeys" {
            return Some(PathBuf::from("/dev/input").join(entry.file_name()));
        }
    }
    None
}

pub fn spawn_button_watcher(
    sender: SyncSender<DaemonEventKind>,
    terminate: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !terminate.load(Ordering::Relaxed) {
            let Some(path) = find_button_device() else {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            };
            let Ok(mut device) = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&path)
            else {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            };
            if let Err(error) = read_events(&mut device, &sender, &terminate) {
                tracing::warn!(%error, path = %path.display(), "button watcher reconnecting");
            }
        }
    })
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

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AxisRange {
    minimum: i32,
    maximum: i32,
}

impl AxisRange {
    fn new(minimum: i32, maximum: i32) -> Option<Self> {
        (minimum < maximum).then_some(Self { minimum, maximum })
    }

    /// Normalize a reported ABS value to the signed stick domain used by the
    /// dead-zone logic. The midpoint is zero even for asymmetric ranges and
    /// values outside the ioctl-reported range are harmlessly clamped.
    fn normalize(self, value: i32) -> i32 {
        let minimum = i64::from(self.minimum);
        let maximum = i64::from(self.maximum);
        let value = i64::from(value).clamp(minimum, maximum);
        let midpoint = minimum + (maximum - minimum) / 2;
        if value <= midpoint {
            let span = midpoint - minimum;
            if span == 0 {
                return 0;
            }
            (-32_768_i64 + (value - minimum) * 32_768 / span) as i32
        } else {
            let span = maximum - midpoint;
            if span == 0 {
                return 0;
            }
            ((value - midpoint) * 32_767 / span) as i32
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct StickCalibration {
    x: Option<AxisRange>,
    y: Option<AxisRange>,
}

impl StickCalibration {
    fn normalize_x(self, value: i32) -> i32 {
        self.x.map_or(0, |range| range.normalize(value))
    }

    fn normalize_y(self, value: i32) -> i32 {
        self.y.map_or(0, |range| range.normalize(value))
    }
}

struct ControllerDevice {
    path: PathBuf,
    file: File,
    class: DeviceClass,
    grabbed: bool,
    held: HashMap<ControllerAction, Instant>,
    stick: StickState,
    /// Set by SYN_DROPPED; events are discarded until the next SYN_REPORT.
    dropping: bool,
    calibration: StickCalibration,
}

/// Grab state reported by the input thread after handling capture request
/// `sequence`; `requested` is that request's enabled flag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureStatus {
    pub sequence: u64,
    pub requested: bool,
    pub grabbed: usize,
    pub total: usize,
}

impl CaptureStatus {
    /// Capture requested and every controller (at least one) is grabbed.
    pub fn captured(&self) -> bool {
        self.requested && self.total > 0 && self.grabbed == self.total
    }

    /// Release requested and no controller remains grabbed.
    pub fn released(&self) -> bool {
        !self.requested && self.grabbed == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CaptureCommand {
    sequence: u64,
    enabled: bool,
}

trait GrabDevice {
    fn is_grabbed(&self) -> bool;
    fn try_set_grabbed(&mut self, grabbed: bool) -> bool;
}

impl GrabDevice for ControllerDevice {
    fn is_grabbed(&self) -> bool {
        self.grabbed
    }

    fn try_set_grabbed(&mut self, grabbed: bool) -> bool {
        set_grabbed(self, grabbed)
    }
}

fn reconcile_grabs<D: GrabDevice>(
    devices: &mut [D],
    sequence: u64,
    requested: bool,
) -> CaptureStatus {
    for device in devices.iter_mut() {
        if device.is_grabbed() != requested {
            device.try_set_grabbed(requested);
        }
    }
    grab_status(devices, sequence, requested)
}

fn grab_status<D: GrabDevice>(devices: &[D], sequence: u64, requested: bool) -> CaptureStatus {
    CaptureStatus {
        sequence,
        requested,
        grabbed: devices.iter().filter(|device| device.is_grabbed()).count(),
        total: devices.len(),
    }
}

pub struct ControllerCaptureHandle {
    /// Commands go over a datagram socket so they wake the input thread's poll.
    control: UnixDatagram,
    pub status: Receiver<CaptureStatus>,
    thread: std::thread::JoinHandle<()>,
    next_sequence: AtomicU64,
}

impl ControllerCaptureHandle {
    pub fn request_capture(&self, enabled: bool) -> io::Result<u64> {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let mut packet = [0_u8; 10];
        packet[0] = CAPTURE_COMMAND;
        packet[1..9].copy_from_slice(&sequence.to_le_bytes());
        packet[9] = u8::from(enabled);
        self.control.send(&packet)?;
        Ok(sequence)
    }

    pub fn stop(self) {
        drop(self.control);
        let _ = self.thread.join();
    }
}

/// Scan and read controllers in the privileged daemon. The GTK process never
/// opens `/dev/input`; it receives only normalized actions over the socket.
/// `capture` is a short-lived lease maintained by the GUI, so a crashed GUI
/// cannot leave controllers exclusively grabbed from games.
pub fn spawn_controller_watcher(
    sender: SyncSender<ControllerAction>,
    terminate: Arc<AtomicBool>,
) -> io::Result<ControllerCaptureHandle> {
    let (control, receiver) = UnixDatagram::pair()?;
    control.set_nonblocking(true)?;
    receiver.set_nonblocking(true)?;
    let (status_tx, status) = mpsc::channel();
    let thread =
        std::thread::spawn(move || controller_loop(sender, receiver, status_tx, terminate));
    Ok(ControllerCaptureHandle {
        control,
        status,
        thread,
        next_sequence: AtomicU64::new(0),
    })
}

fn controller_loop(
    sender: SyncSender<ControllerAction>,
    control: UnixDatagram,
    status: Sender<CaptureStatus>,
    terminate: Arc<AtomicBool>,
) {
    let mut devices = Vec::<ControllerDevice>::new();
    // Scan immediately; `checked_sub` avoids a panic when CLOCK_MONOTONIC is
    // still below SCAN_INTERVAL just after boot.
    let mut last_scan = Instant::now()
        .checked_sub(SCAN_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut capturing = false;
    let mut capture_sequence = 0;
    let mut reported = None::<CaptureStatus>;

    while !terminate.load(Ordering::Relaxed) {
        if last_scan.elapsed() >= SCAN_INTERVAL {
            scan_controllers(&mut devices);
            if capturing {
                for device in &mut devices {
                    if !device.grabbed {
                        drain_device(device);
                    }
                }
            }
            let current = reconcile_grabs(&mut devices, capture_sequence, capturing);
            report_status(&status, &mut reported, current);
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
            && let Some(command) = receive_capture_command(&control)
        {
            capture_sequence = command.sequence;
            capturing = command.enabled;
            for device in &mut devices {
                if capturing && !device.grabbed {
                    drain_device(device);
                    device.dropping = false;
                }
                device.held.clear();
                device.stick = StickState::default();
            }
            // Each command has a fresh sequence, so this status is always sent.
            let current = reconcile_grabs(&mut devices, capture_sequence, capturing);
            report_status(&status, &mut reported, current);
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
        let current = grab_status(&devices, capture_sequence, capturing);
        report_status(&status, &mut reported, current);
        if capturing {
            emit_repeats(&mut devices, &sender);
        }
    }

    let _ = reconcile_grabs(&mut devices, capture_sequence, false);
}

/// Send `current` unless it repeats the last delivered status.
fn report_status(
    sender: &Sender<CaptureStatus>,
    reported: &mut Option<CaptureStatus>,
    current: CaptureStatus,
) {
    if *reported != Some(current) && sender.send(current).is_ok() {
        *reported = Some(current);
        crate::wake::notify();
    }
}

fn receive_capture_command(control: &UnixDatagram) -> Option<CaptureCommand> {
    let mut latest = None;
    let mut packet = [0_u8; 32];
    loop {
        match control.recv(&mut packet) {
            Ok(0) => break,
            Ok(count) => {
                if let Some(command) = decode_capture_command(&packet[..count]) {
                    latest = Some(command);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => {
                tracing::warn!(%error, "controller control channel failed");
                break;
            }
        }
    }
    latest
}

fn decode_capture_command(packet: &[u8]) -> Option<CaptureCommand> {
    if packet.len() != 10 || packet[0] != CAPTURE_COMMAND || packet[9] > 1 {
        return None;
    }
    Some(CaptureCommand {
        sequence: u64::from_le_bytes(packet[1..9].try_into().ok()?),
        enabled: packet[9] != 0,
    })
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

fn scan_controllers(devices: &mut Vec<ControllerDevice>) {
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
        let calibration = if class == DeviceClass::Controller {
            query_stick_calibration(file.as_raw_fd())
        } else {
            StickCalibration::default()
        };
        let device = ControllerDevice {
            path,
            file,
            class,
            grabbed: false,
            held: HashMap::new(),
            stick: StickState::default(),
            dropping: false,
            calibration,
        };
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

const fn eviocgabs(axis: u16) -> libc::c_ulong {
    // `_IOC_NR` occupies the low eight bits; the ioctl type (`'E'`) is the
    // next byte. Advancing an ABS code therefore increments the request
    // number directly rather than shifting into the type field.
    EVIOCGABS_BASE + axis as libc::c_ulong
}

fn read_abs_info(fd: libc::c_int, axis: u16) -> io::Result<InputAbsInfo> {
    let mut info = InputAbsInfo::default();
    // SAFETY: `info` is a writable input_absinfo-compatible buffer and `fd`
    // remains owned by the ControllerDevice for this call.
    let result = unsafe { libc::ioctl(fd, eviocgabs(axis), &mut info) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(info)
    }
}

fn query_stick_calibration_with<F>(mut read_axis: F) -> StickCalibration
where
    F: FnMut(u16) -> io::Result<InputAbsInfo>,
{
    fn query_axis<F>(axis: u16, read_axis: &mut F) -> Option<AxisRange>
    where
        F: FnMut(u16) -> io::Result<InputAbsInfo>,
    {
        match read_axis(axis) {
            Ok(info) => match AxisRange::new(info.minimum, info.maximum) {
                Some(range) => Some(range),
                None => {
                    tracing::warn!(
                        axis,
                        minimum = info.minimum,
                        maximum = info.maximum,
                        "ignoring malformed controller ABS range"
                    );
                    None
                }
            },
            Err(error) => {
                tracing::debug!(%error, axis, "controller ABS range is unavailable");
                None
            }
        }
    }

    StickCalibration {
        x: query_axis(ABS_X, &mut read_axis),
        y: query_axis(ABS_Y, &mut read_axis),
    }
}

fn query_stick_calibration(fd: libc::c_int) -> StickCalibration {
    query_stick_calibration_with(|axis| read_abs_info(fd, axis))
}

fn set_grabbed(device: &mut ControllerDevice, grabbed: bool) -> bool {
    if device.grabbed == grabbed {
        return true;
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
        true
    } else {
        tracing::warn!(
            path = %device.path.display(),
            grabbed,
            error = %io::Error::last_os_error(),
            "controller exclusive capture failed"
        );
        false
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
    sender: &SyncSender<ControllerAction>,
) -> io::Result<()> {
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
        for event in bytes[..count].chunks_exact(EVENT_BYTES) {
            let (event_type, code, value) = decode_event(event);
            handle_controller_event(device, event_type, code, value, sender);
        }
    }
}

fn handle_controller_event(
    device: &mut ControllerDevice,
    event_type: u16,
    code: u16,
    value: i32,
    sender: &SyncSender<ControllerAction>,
) {
    if device.dropping {
        device.dropping = !(event_type == EV_SYN && code == SYN_REPORT);
        return;
    }
    match event_type {
        // The kernel overflowed this client's buffer, so a release may be
        // lost; forget held directions rather than repeat them indefinitely,
        // and discard the incomplete frame up to the next SYN_REPORT.
        EV_SYN if code == SYN_DROPPED => {
            device.held.clear();
            device.stick = StickState::default();
            device.dropping = true;
        }
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
                device.stick.x = device.calibration.normalize_x(value);
            } else {
                device.stick.y = device.calibration.normalize_y(value);
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

fn apply_stick_direction(device: &mut ControllerDevice, sender: &SyncSender<ControllerAction>) {
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
    sender: &SyncSender<ControllerAction>,
) {
    let _ = sender.try_send(action);
    crate::wake::notify();
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

fn emit_repeats(devices: &mut [ControllerDevice], sender: &SyncSender<ControllerAction>) {
    let now = Instant::now();
    for device in devices {
        if !device.grabbed || device.class != DeviceClass::Controller {
            continue;
        }
        for (action, deadline) in &mut device.held {
            if now >= *deadline {
                let _ = sender.try_send(*action);
                crate::wake::notify();
                *deadline = now + REPEAT_INTERVAL;
            }
        }
    }
}

fn read_events(
    device: &mut File,
    sender: &SyncSender<DaemonEventKind>,
    terminate: &AtomicBool,
) -> Result<(), String> {
    let mut event = [0_u8; EVENT_BYTES];
    while !terminate.load(Ordering::Relaxed) {
        let mut poll = libc::pollfd {
            fd: device.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll` receives a pointer to the live local descriptor.
        let ready = unsafe { libc::poll(&mut poll, 1, 500) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.to_string());
        }
        if ready == 0 {
            continue;
        }
        if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err("button input device disconnected".into());
        }
        match device.read_exact(&mut event) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error.to_string()),
        }
        let (event_type, code, value) = decode_event(&event);
        if event_type == EV_KEY && code == KEY_PROG3 && value == 1 {
            let _ = sender.try_send(DaemonEventKind::GuiToggle);
            crate::wake::notify();
        }
    }
    Ok(())
}

/// Split one `EVENT_BYTES` input_event into (type, code, value).
fn decode_event(event: &[u8]) -> (u16, u16, i32) {
    let offset = std::mem::size_of::<libc::timeval>();
    (
        u16::from_ne_bytes(event[offset..offset + 2].try_into().unwrap()),
        u16::from_ne_bytes(event[offset + 2..offset + 4].try_into().unwrap()),
        i32::from_ne_bytes(event[offset + 4..offset + 8].try_into().unwrap()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stick_behavior_covers_deadzone_hysteresis_and_axis_switching() {
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
        sender
            .send(&[CAPTURE_COMMAND, 7, 0, 0, 0, 0, 0, 0, 0, 1])
            .unwrap();
        sender
            .send(&[CAPTURE_COMMAND, 8, 0, 0, 0, 0, 0, 0, 0, 0])
            .unwrap();
        assert_eq!(
            receive_capture_command(&receiver),
            Some(CaptureCommand {
                sequence: 8,
                enabled: false
            })
        );
        assert_eq!(receive_capture_command(&receiver), None);
    }

    #[derive(Default)]
    struct FakeGrabDevice {
        grabbed: bool,
        fail_grab: bool,
        fail_ungrab: bool,
        calls: Vec<bool>,
    }

    impl GrabDevice for FakeGrabDevice {
        fn is_grabbed(&self) -> bool {
            self.grabbed
        }

        fn try_set_grabbed(&mut self, grabbed: bool) -> bool {
            self.calls.push(grabbed);
            if (grabbed && self.fail_grab) || (!grabbed && self.fail_ungrab) {
                return false;
            }
            self.grabbed = grabbed;
            true
        }
    }

    #[test]
    fn failed_ungrab_keeps_partial_cleanup_state_for_retry() {
        let mut devices = vec![FakeGrabDevice {
            grabbed: true,
            fail_ungrab: true,
            ..FakeGrabDevice::default()
        }];
        let status = reconcile_grabs(&mut devices, 12, false);
        assert_eq!(
            status,
            CaptureStatus {
                sequence: 12,
                requested: false,
                grabbed: 1,
                total: 1,
            }
        );
        assert!(!status.captured() && !status.released());
        devices[0].fail_ungrab = false;
        let status = reconcile_grabs(&mut devices, 12, false);
        assert_eq!(
            status,
            CaptureStatus {
                sequence: 12,
                requested: false,
                grabbed: 0,
                total: 1,
            }
        );
        assert!(status.released());
    }

    #[test]
    fn failed_grab_of_hotplugged_device_is_explicitly_partial_and_never_captured() {
        let mut devices = vec![FakeGrabDevice::default()];
        let first = reconcile_grabs(&mut devices, 13, true);
        assert_eq!((first.grabbed, first.total), (1, 1));
        assert!(first.captured());
        devices.push(FakeGrabDevice {
            fail_grab: true,
            ..FakeGrabDevice::default()
        });
        let partial = reconcile_grabs(&mut devices, 13, true);
        assert_eq!(
            partial,
            CaptureStatus {
                sequence: 13,
                requested: true,
                grabbed: 1,
                total: 2,
            }
        );
        assert!(!partial.captured() && !partial.released());
    }

    fn fake_abs_info(minimum: i32, maximum: i32) -> InputAbsInfo {
        InputAbsInfo {
            minimum,
            maximum,
            ..InputAbsInfo::default()
        }
    }

    #[test]
    fn normalizes_asymmetric_ranges_and_clamps_ioctl_values() {
        let range = AxisRange::new(100, 500).unwrap();
        assert_eq!(range.normalize(100), -32_768);
        assert_eq!(range.normalize(300), 0);
        assert_eq!(range.normalize(500), 32_767);
        assert_eq!(range.normalize(i32::MIN), -32_768);
        assert_eq!(range.normalize(i32::MAX), 32_767);

        let calibration = StickCalibration {
            x: Some(range),
            y: Some(AxisRange::new(-20, 30).unwrap()),
        };
        assert_eq!(calibration.normalize_x(300), 0);
        assert_eq!(calibration.normalize_y(-20), -32_768);
    }

    #[test]
    fn rejects_degenerate_or_reversed_abs_ranges_without_disabling_other_axis() {
        assert_eq!(AxisRange::new(7, 7), None);
        assert_eq!(AxisRange::new(8, 7), None);

        // The reported value is only a snapshot and normalize clamps it, so it
        // must not cost the axis its calibration.
        let value_outside_range = query_stick_calibration_with(|_| {
            Ok(InputAbsInfo {
                value: 256,
                minimum: 0,
                maximum: 255,
                fuzz: -1,
                ..InputAbsInfo::default()
            })
        });
        assert_eq!(value_outside_range.x, AxisRange::new(0, 255));
        assert_eq!(value_outside_range.normalize_y(256), 32_767);

        let calibration = query_stick_calibration_with(|axis| {
            if axis == ABS_X {
                Ok(fake_abs_info(7, 7))
            } else {
                Ok(fake_abs_info(-10, 10))
            }
        });
        assert_eq!(calibration.x, None);
        assert_eq!(calibration.y, Some(AxisRange::new(-10, 10).unwrap()));
        assert_eq!(calibration.normalize_x(i32::MAX), 0);
        assert_eq!(calibration.normalize_y(10), 32_767);
    }

    #[test]
    fn ioctl_errors_are_isolated_per_axis_and_hotplug_queries_are_fresh() {
        let unavailable = query_stick_calibration_with(|axis| {
            if axis == ABS_X {
                Err(io::Error::from(io::ErrorKind::NotFound))
            } else {
                Ok(fake_abs_info(0, 255))
            }
        });
        assert_eq!(unavailable.x, None);
        assert_eq!(unavailable.y, Some(AxisRange::new(0, 255).unwrap()));

        // A newly opened event node gets a new ioctl query rather than
        // inheriting the range of the device that used the path before it.
        let first = query_stick_calibration_with(|_| Ok(fake_abs_info(0, 255)));
        let replacement = query_stick_calibration_with(|_| Ok(fake_abs_info(-1_000, 1_000)));
        assert_ne!(first, replacement);
        assert_eq!(replacement.x.unwrap().minimum, -1_000);
        assert_eq!(replacement.y.unwrap().maximum, 1_000);
    }

    #[test]
    fn dropped_events_release_held_directions_and_stick_state() {
        let (sender, _receiver) = std::sync::mpsc::sync_channel(8);
        let range = AxisRange::new(-10, 10);
        let mut device = ControllerDevice {
            path: PathBuf::from("/dev/null"),
            file: File::open("/dev/null").unwrap(),
            class: DeviceClass::Controller,
            grabbed: true,
            held: HashMap::new(),
            stick: StickState::default(),
            dropping: false,
            calibration: StickCalibration { x: range, y: range },
        };
        handle_controller_event(&mut device, EV_ABS, ABS_HAT0Y, -1, &sender);
        handle_controller_event(&mut device, EV_ABS, ABS_X, -10, &sender);
        assert_eq!(device.held.len(), 2);
        // The release may be among the events lost to the buffer overflow.
        handle_controller_event(&mut device, EV_SYN, SYN_DROPPED, 0, &sender);
        assert!(device.held.is_empty());
        assert_eq!(device.stick, StickState::default());
        // Events up to and including the next SYN_REPORT are incomplete.
        handle_controller_event(&mut device, EV_ABS, ABS_HAT0Y, -1, &sender);
        handle_controller_event(&mut device, EV_SYN, SYN_REPORT, 0, &sender);
        assert!(device.held.is_empty());
        handle_controller_event(&mut device, EV_ABS, ABS_HAT0Y, -1, &sender);
        assert!(device.held.contains_key(&ControllerAction::Up));
    }

    #[test]
    fn linux_input_constants_and_ioctl_match_abi() {
        assert_eq!(EV_KEY, 1);
        assert_eq!(KEY_PROG3, 202);
        assert_eq!(ABS_X, 0);
        assert_eq!(ABS_Y, 1);
        assert_eq!(BTN_SOUTH, 0x130);
        assert_eq!(BTN_EAST, 0x131);
        assert_eq!(std::mem::size_of::<InputAbsInfo>(), 24);
        assert_eq!(eviocgabs(ABS_X), 0x8018_4540);
        assert_eq!(eviocgabs(ABS_Y), 0x8018_4541);
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
