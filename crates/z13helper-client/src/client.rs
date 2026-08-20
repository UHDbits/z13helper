use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use z13helper_core::curve::Curve;
use z13helper_core::error::{DaemonError, ErrorCode};
use z13helper_core::protocol::{
    ApplyRequest, ApplyResponse, Command, DaemonEvent, DaemonState, LightingState,
    PROTOCOL_VERSION, ProbeReply, WireRequest, WireResponse,
};

const DEFAULT_SOCKET: &str = "/run/z13helper/z13helperd.sock";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FRAME_BYTES: usize = 64 * 1024;
const EVENT_BUFFER_CAPACITY: usize = 64;

#[derive(Clone, Debug)]
pub struct Client {
    socket_path: PathBuf,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        Self::with_path(DEFAULT_SOCKET)
    }

    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: path.into(),
        }
    }

    fn dial(&self) -> Result<UnixStream, DaemonError> {
        let stream = UnixStream::connect(&self.socket_path).map_err(classify_dial_error)?;
        stream
            .set_read_timeout(Some(COMMAND_TIMEOUT))
            .and_then(|_| stream.set_write_timeout(Some(COMMAND_TIMEOUT)))
            .map_err(map_io_error)?;
        Ok(stream)
    }

    fn exchange(&self, command: Command) -> Result<WireResponse, DaemonError> {
        let mut stream = self.dial()?;
        let request = WireRequest {
            version: PROTOCOL_VERSION,
            command,
        };
        write_frame(&mut stream, &request)?;
        let mut reader = BufReader::new(stream);
        let line = read_frame(&mut reader)?;
        let response: WireResponse = serde_json::from_str(&line)
            .map_err(|error| DaemonError::Protocol(format!("invalid response JSON: {error}")))?;
        if response.ok {
            Ok(response)
        } else {
            Err(map_wire_error(response))
        }
    }

    pub fn get_state(&self) -> Result<DaemonState, DaemonError> {
        self.exchange(Command::GetState)?
            .state
            .ok_or_else(|| DaemonError::Protocol("missing state in response".into()))
    }

    pub fn probe(&self) -> Result<ProbeReply, DaemonError> {
        self.exchange(Command::Probe)?
            .probe
            .ok_or_else(|| DaemonError::Protocol("missing probe in response".into()))
    }

    pub fn apply(&self, request: ApplyRequest) -> Result<ApplyResponse, DaemonError> {
        self.exchange(Command::Apply { request })?
            .apply
            .ok_or_else(|| DaemonError::Protocol("missing apply response".into()))
    }

    pub fn apply_undervolt_once(&self, offset: i32) -> Result<(), DaemonError> {
        self.exchange(Command::ApplyUndervoltOnce { offset })?;
        Ok(())
    }

    pub fn factory_fan_curves(
        &self,
        ppd_profiles: Vec<String>,
    ) -> Result<HashMap<String, [Curve; 2]>, DaemonError> {
        self.exchange(Command::GetFactoryFanCurves { ppd_profiles })?
            .factory_fan_curves
            .ok_or_else(|| DaemonError::Protocol("missing factory fan curves".into()))
    }

    pub fn battery_limit_set(&self, limit: i32) -> Result<(), DaemonError> {
        self.exchange(Command::SetBatteryLimit { limit })?;
        Ok(())
    }

    pub fn battery_one_time_charge_set(&self, enabled: bool) -> Result<(), DaemonError> {
        self.exchange(Command::SetBatteryOneTimeCharge { enabled })?;
        Ok(())
    }

    pub fn panel_overdrive_set(&self, value: i32) -> Result<(), DaemonError> {
        self.exchange(Command::SetPanelOverdrive {
            enabled: value != 0,
        })?;
        Ok(())
    }

    pub fn apply_lighting(
        &self,
        mode: &str,
        color: &str,
        color2: &str,
        speed: &str,
        brightness: i32,
        device: &str,
    ) -> Result<(), DaemonError> {
        self.exchange(Command::SetLighting {
            device: device.into(),
            state: LightingState {
                enabled: true,
                mode: mode.into(),
                color: color.into(),
                color2: color2.into(),
                speed: speed.into(),
                brightness,
            },
        })?;
        Ok(())
    }

    pub fn lighting_off(&self, device: &str) -> Result<(), DaemonError> {
        self.exchange(Command::SetLighting {
            device: device.into(),
            state: LightingState {
                enabled: false,
                ..LightingState::default()
            },
        })?;
        Ok(())
    }

    pub fn release_fans(&self) -> Result<(), DaemonError> {
        self.exchange(Command::ReleaseFans)?;
        Ok(())
    }

    pub fn set_controller_capture(&self, enabled: bool) -> Result<(), DaemonError> {
        self.exchange(Command::SetControllerCapture { enabled })?;
        Ok(())
    }

    pub fn subscribe(
        &self,
        events: &[&str],
    ) -> Result<(std::sync::mpsc::Receiver<DaemonEvent>, SubscribeCancel), DaemonError> {
        let mut stream = self.dial()?;
        let request = WireRequest {
            version: PROTOCOL_VERSION,
            command: Command::Subscribe {
                events: events.iter().map(|event| (*event).into()).collect(),
            },
        };
        write_frame(&mut stream, &request)?;
        let mut reader = BufReader::new(stream);
        let ack = read_frame(&mut reader)?;
        let response: WireResponse =
            serde_json::from_str(&ack).map_err(|error| DaemonError::Protocol(error.to_string()))?;
        if !response.ok {
            return Err(map_wire_error(response));
        }
        reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(map_io_error)?;
        let (tx, rx) = std::sync::mpsc::sync_channel(EVENT_BUFFER_CAPACITY);
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            loop {
                if cancel_rx.try_recv().is_ok() {
                    break;
                }
                match read_frame(&mut reader) {
                    Ok(line) => {
                        if let Ok(response) = serde_json::from_str::<WireResponse>(&line)
                            && let Some(event) = response.event
                            && let Err(error) = tx.try_send(event)
                            && matches!(error, std::sync::mpsc::TrySendError::Disconnected(_))
                        {
                            break;
                        }
                    }
                    Err(DaemonError::Timeout) => {}
                    Err(_) => break,
                }
            }
        });
        Ok((rx, SubscribeCancel { tx: cancel_tx }))
    }
}

pub struct SubscribeCancel {
    tx: std::sync::mpsc::Sender<()>,
}

impl SubscribeCancel {
    pub fn cancel(self) {
        let _ = self.tx.send(());
    }
}

impl Drop for SubscribeCancel {
    fn drop(&mut self) {
        let _ = self.tx.send(());
    }
}

fn write_frame(stream: &mut impl Write, value: &WireRequest) -> Result<(), DaemonError> {
    let body =
        serde_json::to_vec(value).map_err(|error| DaemonError::Protocol(error.to_string()))?;
    if body.len() + 1 > MAX_FRAME_BYTES {
        return Err(DaemonError::Protocol(
            "outgoing frame exceeds 64 KiB".into(),
        ));
    }
    stream.write_all(&body).map_err(map_io_error)?;
    stream.write_all(b"\n").map_err(map_io_error)
}

fn read_frame(reader: &mut impl BufRead) -> Result<String, DaemonError> {
    let mut bytes = Vec::new();
    let read = reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(map_io_error)?;
    if read == 0 {
        return Err(DaemonError::Protocol("no response from daemon".into()));
    }
    if bytes.last() != Some(&b'\n') || bytes.len() > MAX_FRAME_BYTES {
        return Err(DaemonError::Protocol(
            "incoming frame exceeds 64 KiB".into(),
        ));
    }
    bytes.pop();
    String::from_utf8(bytes).map_err(|error| DaemonError::Protocol(error.to_string()))
}

fn map_wire_error(response: WireResponse) -> DaemonError {
    let Some(error) = response.error else {
        return DaemonError::Protocol("daemon returned failure without an error".into());
    };
    match error.code {
        ErrorCode::NotRunning => DaemonError::NotRunning,
        ErrorCode::PermissionDenied => DaemonError::PermissionDenied,
        ErrorCode::Timeout => DaemonError::Timeout,
        ErrorCode::Protocol => DaemonError::Protocol(error.message),
        ErrorCode::Rejected | ErrorCode::Unsupported | ErrorCode::Degraded => {
            DaemonError::Rejected(error.message)
        }
    }
}

fn classify_dial_error(error: std::io::Error) -> DaemonError {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => DaemonError::PermissionDenied,
        std::io::ErrorKind::NotFound
        | std::io::ErrorKind::ConnectionRefused
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::TimedOut => DaemonError::NotRunning,
        _ => DaemonError::Protocol(error.to_string()),
    }
}

fn map_io_error(error: std::io::Error) -> DaemonError {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => DaemonError::Timeout,
        std::io::ErrorKind::PermissionDenied => DaemonError::PermissionDenied,
        _ => DaemonError::Protocol(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};
    use std::os::unix::net::UnixListener;

    #[test]
    fn missing_socket_error_is_not_running() {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(classify_dial_error(error), DaemonError::NotRunning);
    }

    #[test]
    fn frames_are_bounded_in_both_directions() {
        let mut reader = BufReader::new(Cursor::new(vec![b'x'; MAX_FRAME_BYTES + 1]));
        assert!(matches!(
            read_frame(&mut reader),
            Err(DaemonError::Protocol(message)) if message.contains("exceeds")
        ));

        let request = WireRequest {
            version: PROTOCOL_VERSION,
            command: Command::Subscribe {
                events: vec!["x".repeat(MAX_FRAME_BYTES)],
            },
        };
        assert!(matches!(
            write_frame(&mut Vec::new(), &request),
            Err(DaemonError::Protocol(message)) if message.contains("exceeds")
        ));
    }

    #[test]
    fn get_state_uses_versioned_protocol() {
        let dir = std::env::temp_dir().join(format!("z13helper-client-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("daemon.sock");
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            // Some CI sandboxes prohibit AF_UNIX entirely. Error mapping is
            // covered above; run this transport test on an unrestricted host.
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("bind {}: {error}", path.display()),
        };
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            assert!(request.contains("\"version\":2"));
            assert!(request.contains("\"cmd\":\"get-state\""));
            let response = WireResponse {
                ok: true,
                state: Some(DaemonState::default()),
                apply: None,
                probe: None,
                factory_fan_curves: None,
                event: None,
                error: None,
            };
            let mut stream = stream;
            writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        });
        let state = Client::with_path(path).get_state().unwrap();
        assert_eq!(state.ppd_profile.as_deref(), Some("balanced"));
    }

    #[test]
    fn one_time_charge_uses_persistent_daemon_command() {
        let dir = std::env::temp_dir().join(format!(
            "z13helper-client-charge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("daemon.sock");
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("bind {}: {error}", path.display()),
        };
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            assert!(request.contains("\"cmd\":\"set-battery-one-time-charge\""));
            assert!(request.contains("\"enabled\":true"));
            let mut stream = stream;
            writeln!(
                stream,
                "{}",
                serde_json::to_string(&WireResponse::success()).unwrap()
            )
            .unwrap();
        });
        Client::with_path(path)
            .battery_one_time_charge_set(true)
            .unwrap();
    }

    #[test]
    fn one_shot_undervolt_uses_non_persistent_daemon_command() {
        let dir = std::env::temp_dir().join(format!(
            "z13helper-client-uv-once-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("daemon.sock");
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("bind {}: {error}", path.display()),
        };
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            assert!(request.contains("\"cmd\":\"apply-undervolt-once\""));
            assert!(request.contains("\"offset\":-20"));
            let mut stream = stream;
            writeln!(
                stream,
                "{}",
                serde_json::to_string(&WireResponse::success()).unwrap()
            )
            .unwrap();
        });
        Client::with_path(path).apply_undervolt_once(-20).unwrap();
    }

    #[test]
    fn factory_fan_curves_are_returned_by_ppd_profile() {
        let dir = std::env::temp_dir().join(format!(
            "z13helper-client-factory-fans-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("daemon.sock");
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("bind {}: {error}", path.display()),
        };
        let expected = z13helper_core::stock_fan_curves(Some("balanced"));
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            assert!(request.contains("\"cmd\":\"get-factory-fan-curves\""));
            assert!(request.contains("\"balanced\""));
            let mut curves = HashMap::new();
            curves.insert("balanced".into(), expected);
            let mut response = WireResponse::success();
            response.factory_fan_curves = Some(curves);
            let mut stream = stream;
            writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        });
        let curves = Client::with_path(path)
            .factory_fan_curves(vec!["balanced".into()])
            .unwrap();
        assert_eq!(curves["balanced"], expected);
    }
}
