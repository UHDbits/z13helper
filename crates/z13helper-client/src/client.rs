use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use z13helper_core::curve::Curve;
use z13helper_core::error::{DaemonError, ErrorCode};
use z13helper_core::protocol::{
    ApplyRequest, ApplyResponse, ClientId, Command, DaemonEvent, EventTopic, LightingState,
    MAX_FRAME_BYTES as PROTOCOL_MAX_FRAME_BYTES, PROTOCOL_VERSION, ProbeReply, RequestId,
    RequestOutcome, WireRequest, WireResponse, WireStatus,
};

const DEFAULT_SOCKET: &str = "/run/z13helper/z13helperd.sock";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_BUFFER_CAPACITY: usize = 64;
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static CLIENT_ID: std::sync::OnceLock<ClientId> = std::sync::OnceLock::new();

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
    /// Maximum size of a single newline-delimited protocol frame.
    ///
    /// This is also used by the CLI when reading JSON from stdin, so callers
    /// do not need to duplicate the transport limit.
    pub const MAX_FRAME_BYTES: usize = PROTOCOL_MAX_FRAME_BYTES;

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

    fn next_request_id() -> RequestId {
        loop {
            let value = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            if let Some(request_id) = RequestId::new(value) {
                return request_id;
            }
        }
    }

    fn client_id() -> ClientId {
        *CLIENT_ID.get_or_init(|| {
            let mut bytes = [0_u8; 16];
            let random = std::fs::File::open("/dev/urandom")
                .and_then(|mut file| file.read_exact(&mut bytes))
                .ok()
                .and_then(|()| ClientId::new(u128::from_ne_bytes(bytes)));
            random.unwrap_or_else(|| {
                // Linux always provides /dev/urandom, but retain a non-zero,
                // process-distinct fallback for constrained test containers.
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let fallback = nanos ^ (u128::from(std::process::id()) << 96);
                ClientId::new(fallback).unwrap_or_else(|| ClientId::new(1).unwrap())
            })
        })
    }

    fn exchange(&self, command: Command) -> Result<WireResponse, DaemonError> {
        self.exchange_with_id(Self::next_request_id(), command, true)
    }

    fn exchange_with_id(
        &self,
        request_id: RequestId,
        command: Command,
        wait_for_completion: bool,
    ) -> Result<WireResponse, DaemonError> {
        let outcome_recoverable = !matches!(command, Command::GetOutcome { .. });
        let mut stream = self.dial()?;
        let request = WireRequest {
            version: PROTOCOL_VERSION,
            client_id: Self::client_id(),
            request_id,
            command,
        };
        let frame = encode_request(&request)?;
        if let Err(error) = stream.write_all(&frame) {
            // write_all may have delivered a complete request before the
            // transport error became visible. Conservatively preserve the
            // correlation ID instead of inviting a duplicate mutation.
            return Err(if outcome_recoverable {
                outcome_unknown(request_id)
            } else {
                map_io_error(error)
            });
        }
        let mut reader = BufReader::new(stream);
        loop {
            let line = match read_frame(&mut reader) {
                Ok(line) => line,
                Err(DaemonError::Timeout) if outcome_recoverable => {
                    return Err(outcome_unknown(request_id));
                }
                Err(DaemonError::Protocol(message))
                    if outcome_recoverable && message == "no response from daemon" =>
                {
                    return Err(outcome_unknown(request_id));
                }
                Err(error) => return Err(error),
            };
            let response: WireResponse = serde_json::from_str(&line).map_err(|error| {
                DaemonError::Protocol(format!("invalid response JSON: {error}"))
            })?;
            if response.version != PROTOCOL_VERSION {
                return Err(DaemonError::Protocol(format!(
                    "unsupported response version {}; expected {PROTOCOL_VERSION}",
                    response.version
                )));
            }
            if response.request_id != Some(request_id) {
                return Err(DaemonError::Protocol(format!(
                    "response request ID {:?} does not match {}",
                    response.request_id,
                    request_id.get()
                )));
            }
            if wait_for_completion
                && matches!(
                    response.outcome,
                    RequestOutcome::Queued | RequestOutcome::Started
                )
            {
                continue;
            }
            if response.ok {
                return Ok(response);
            }
            return Err(map_wire_error(response));
        }
    }

    pub fn get_state(&self) -> Result<WireStatus, DaemonError> {
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

    pub fn panel_overdrive_set(&self, enabled: bool) -> Result<(), DaemonError> {
        self.exchange(Command::SetPanelOverdrive { enabled })?;
        Ok(())
    }

    pub fn apply_lighting(&self, device: &str, state: LightingState) -> Result<(), DaemonError> {
        self.exchange(Command::SetLighting {
            device: device.into(),
            state,
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

    /// Look up an accepted request using the complete recovery identity shown
    /// by [`DaemonError::OutcomeUnknown`]. This supports a new process after
    /// the process that submitted the request has exited.
    pub fn get_outcome_for(
        &self,
        client_id: ClientId,
        request_id: u64,
    ) -> Result<WireResponse, DaemonError> {
        let target =
            RequestId::try_from(request_id).map_err(|error| DaemonError::Protocol(error.into()))?;
        let response = self.exchange_with_id(
            Self::next_request_id(),
            Command::GetOutcome {
                target_client_id: client_id,
                target_request_id: target,
            },
            false,
        )?;
        if response.outcome_client_id != Some(client_id)
            || response.outcome_request_id != Some(target)
        {
            return Err(DaemonError::Protocol(
                "outcome response omitted the requested client or request ID".into(),
            ));
        }
        Ok(response)
    }

    pub fn subscribe(
        &self,
        events: &[EventTopic],
    ) -> Result<(std::sync::mpsc::Receiver<DaemonEvent>, SubscribeCancel), DaemonError> {
        let mut stream = self.dial()?;
        let request_id = Self::next_request_id();
        let request = WireRequest {
            version: PROTOCOL_VERSION,
            client_id: Self::client_id(),
            request_id,
            command: Command::Subscribe {
                events: events.to_vec(),
            },
        };
        write_frame(&mut stream, &request)?;
        let mut reader = BufReader::new(stream);
        let ack = read_frame(&mut reader)?;
        let response: WireResponse =
            serde_json::from_str(&ack).map_err(|error| DaemonError::Protocol(error.to_string()))?;
        if response.version != PROTOCOL_VERSION || response.request_id != Some(request_id) {
            return Err(DaemonError::Protocol(
                "subscription response correlation mismatch".into(),
            ));
        }
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
                        {
                            match tx.try_send(event) {
                                Ok(()) => {}
                                // Events are notifications, not an authoritative
                                // state log. Under a stalled consumer, retain the
                                // bounded memory guarantee and let the next status
                                // reconciliation recover coalescible state.
                                Err(std::sync::mpsc::TrySendError::Full(_)) => {}
                                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => break,
                            }
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

fn outcome_unknown(request_id: RequestId) -> DaemonError {
    DaemonError::OutcomeUnknown {
        client_id: Client::client_id(),
        request_id: request_id.get(),
    }
}

pub struct SubscribeCancel {
    tx: std::sync::mpsc::Sender<()>,
}

impl Drop for SubscribeCancel {
    fn drop(&mut self) {
        let _ = self.tx.send(());
    }
}

fn write_frame(stream: &mut impl Write, value: &WireRequest) -> Result<(), DaemonError> {
    let frame = encode_request(value)?;
    stream.write_all(&frame).map_err(map_io_error)
}

fn encode_request(value: &WireRequest) -> Result<Vec<u8>, DaemonError> {
    let body =
        serde_json::to_vec(value).map_err(|error| DaemonError::Protocol(error.to_string()))?;
    if body.len() + 1 > PROTOCOL_MAX_FRAME_BYTES {
        return Err(DaemonError::Protocol(format!(
            "outgoing frame exceeds {PROTOCOL_MAX_FRAME_BYTES} bytes"
        )));
    }
    let mut frame = body;
    frame.push(b'\n');
    Ok(frame)
}

fn read_frame(reader: &mut impl BufRead) -> Result<String, DaemonError> {
    let mut bytes = Vec::new();
    let read = reader
        .take((PROTOCOL_MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(map_io_error)?;
    if read == 0 {
        return Err(DaemonError::Protocol("no response from daemon".into()));
    }
    if bytes.last() != Some(&b'\n') || bytes.len() > PROTOCOL_MAX_FRAME_BYTES {
        return Err(DaemonError::Protocol(format!(
            "incoming frame exceeds {PROTOCOL_MAX_FRAME_BYTES} bytes"
        )));
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

    fn assert_command(
        check: impl FnOnce(Command) + Send + 'static,
        invoke: impl FnOnce(Client) -> Result<(), DaemonError>,
    ) {
        let dir = std::env::temp_dir().join(format!(
            "z13helper-client-command-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: WireRequest = serde_json::from_str(line.trim()).unwrap();
            let request_id = request.request_id;
            check(request.command);
            let mut stream = stream;
            writeln!(
                stream,
                "{}",
                serde_json::to_string(&WireResponse::success(request_id)).unwrap()
            )
            .unwrap();
        });
        invoke(Client::with_path(path)).unwrap();
    }

    #[test]
    fn missing_socket_error_is_not_running() {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(classify_dial_error(error), DaemonError::NotRunning);
    }

    #[test]
    fn a_closed_response_exposes_outcome_unknown_request_id() {
        let directory = std::env::temp_dir().join(format!(
            "z13helper-client-outcome-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("daemon.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
        });
        let error = Client::with_path(&path).get_state().unwrap_err();
        assert!(matches!(
            error,
            DaemonError::OutcomeUnknown {
                client_id,
                request_id,
            } if client_id.get() != 0 && request_id != 0
        ));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn outcome_lookup_returns_progress_with_target_correlation() {
        let target_client_id = ClientId::new(9).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "z13helper-client-lookup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("daemon.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let request: WireRequest = serde_json::from_str(request.trim()).unwrap();
            let target = RequestId::new(77).unwrap();
            let mut response = WireResponse::progress(request.request_id, RequestOutcome::Started);
            let Command::GetOutcome {
                target_client_id,
                target_request_id,
            } = request.command
            else {
                panic!("expected outcome query")
            };
            assert_eq!(target_request_id, target);
            response.outcome_client_id = Some(target_client_id);
            response.outcome_request_id = Some(target);
            let mut stream = stream;
            writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        });
        let response = Client::with_path(&path)
            .get_outcome_for(target_client_id, 77)
            .unwrap();
        assert_eq!(response.outcome, RequestOutcome::Started);
        assert_eq!(response.outcome_client_id, Some(target_client_id));
        assert_eq!(
            response.outcome_request_id,
            Some(RequestId::new(77).unwrap())
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn failed_outcome_lookup_is_retryable_without_an_uncacheable_recovery_token() {
        let directory = std::env::temp_dir().join(format!(
            "z13helper-client-lookup-close-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("daemon.sock");
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
        });
        let error = Client::with_path(&path)
            .get_outcome_for(ClientId::new(9).unwrap(), 77)
            .unwrap_err();
        assert!(!matches!(error, DaemonError::OutcomeUnknown { .. }));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn frames_are_bounded_in_both_directions() {
        let mut reader = BufReader::new(Cursor::new(vec![b'x'; PROTOCOL_MAX_FRAME_BYTES + 1]));
        assert!(matches!(
            read_frame(&mut reader),
            Err(DaemonError::Protocol(message)) if message.contains("exceeds")
        ));

        let request = WireRequest {
            version: PROTOCOL_VERSION,
            client_id: ClientId::new(1).unwrap(),
            request_id: RequestId::new(99).unwrap(),
            command: Command::GetFactoryFanCurves {
                ppd_profiles: vec!["x".repeat(PROTOCOL_MAX_FRAME_BYTES)],
            },
        };
        assert!(matches!(
            write_frame(&mut Vec::new(), &request),
            Err(DaemonError::Protocol(message)) if message.contains("exceeds")
        ));
    }

    #[test]
    fn exact_frame_boundary_is_accepted() {
        let mut bytes = vec![b'x'; PROTOCOL_MAX_FRAME_BYTES - 1];
        bytes.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(bytes));
        let frame = read_frame(&mut reader).unwrap();
        assert_eq!(frame.len(), PROTOCOL_MAX_FRAME_BYTES - 1);
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
            assert!(request.contains("\"version\":3"));
            assert!(request.contains("\"cmd\":\"get-state\""));
            let request_id = serde_json::from_str::<WireRequest>(request.trim())
                .unwrap()
                .request_id;
            let state = WireStatus {
                ppd_profile: Some("balanced".into()),
                ..WireStatus::default()
            };
            let response = WireResponse {
                version: PROTOCOL_VERSION,
                request_id: Some(request_id),
                outcome: RequestOutcome::Completed,
                outcome_client_id: None,
                outcome_request_id: None,
                ok: true,
                state: Some(state),
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
    fn scalar_commands_use_typed_daemon_requests() {
        assert_command(
            |command| {
                assert!(matches!(
                    command,
                    Command::SetBatteryOneTimeCharge { enabled: true }
                ))
            },
            |client| client.battery_one_time_charge_set(true),
        );
        assert_command(
            |command| {
                assert!(matches!(
                    command,
                    Command::SetPanelOverdrive { enabled: true }
                ))
            },
            |client| client.panel_overdrive_set(true),
        );
        let expected = LightingState {
            enabled: false,
            mode: "static".into(),
            color: "112233".into(),
            color2: "445566".into(),
            speed: "slow".into(),
            brightness: 1,
        };
        let expected_for_check = expected.clone();
        assert_command(
            move |command| {
                assert!(
                    matches!(command, Command::SetLighting { device, state } if device == "keyboard" && state == expected_for_check)
                )
            },
            move |client| client.apply_lighting("keyboard", expected),
        );
        assert_command(
            |command| {
                assert!(matches!(
                    command,
                    Command::ApplyUndervoltOnce { offset: -20 }
                ))
            },
            |client| client.apply_undervolt_once(-20),
        );
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
            let request_id = serde_json::from_str::<WireRequest>(request.trim())
                .unwrap()
                .request_id;
            let mut curves = HashMap::new();
            curves.insert("balanced".into(), expected);
            let mut response = WireResponse::success(request_id);
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
