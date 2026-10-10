use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use z13helper_core::curve::Curve;
use z13helper_core::error::{DaemonError, ErrorCode};
use z13helper_core::protocol::{
    ApplyRequest, ApplyResponse, ClientId, Command, DaemonEvent, EventTopic, MAX_FRAME_BYTES,
    PROTOCOL_VERSION, ProbeReply, RequestId, RequestOutcome, WireRequest, WireResponse, WireStatus,
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
            // RandomState keys come from OS entropy; two hashes make 128 bits.
            let hash = || u128::from(RandomState::new().hash_one(()));
            ClientId::new((hash() << 64 | hash()).max(1)).expect("non-zero")
        })
    }

    fn request(command: Command) -> WireRequest {
        WireRequest {
            version: PROTOCOL_VERSION,
            client_id: Self::client_id(),
            request_id: Self::next_request_id(),
            command,
        }
    }

    fn exchange(&self, command: Command) -> Result<WireResponse, DaemonError> {
        // A lookup only reads: it returns queued/started progress as its answer
        // and has no outcome of its own to lose.
        let lookup = matches!(command, Command::GetOutcome { .. });
        let mut stream = self.dial()?;
        let request = Self::request(command);
        let request_id = request.request_id;
        let frame = encode_request(&request)?;
        if let Err(error) = stream.write_all(&frame) {
            // write_all may have delivered a complete request before the
            // transport error became visible. Conservatively preserve the
            // correlation ID instead of inviting a duplicate mutation.
            return Err(if lookup {
                map_io_error(error)
            } else {
                outcome_unknown(request_id)
            });
        }
        let mut reader = BufReader::new(stream);
        loop {
            // Once the request is sent, any failed read (timeout, reset, EOF,
            // cut-off or oversized reply) leaves its outcome unknown.
            let line = read_frame(&mut reader, &mut Vec::new()).map_err(|error| {
                if lookup {
                    error
                } else {
                    outcome_unknown(request_id)
                }
            })?;
            let response = parse_reply(&line, request_id)?;
            if !lookup
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

    /// Sends a command whose reply carries no payload.
    pub fn send(&self, command: Command) -> Result<(), DaemonError> {
        self.exchange(command).map(|_| ())
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

    pub fn factory_fan_curves(
        &self,
        ppd_profiles: Vec<String>,
    ) -> Result<HashMap<String, [Curve; 2]>, DaemonError> {
        self.exchange(Command::GetFactoryFanCurves { ppd_profiles })?
            .factory_fan_curves
            .ok_or_else(|| DaemonError::Protocol("missing factory fan curves".into()))
    }

    /// Look up an accepted request using the complete recovery identity shown
    /// by [`DaemonError::OutcomeUnknown`]. This supports a new process after
    /// the process that submitted the request has exited.
    pub fn get_outcome_for(
        &self,
        client_id: ClientId,
        target: RequestId,
    ) -> Result<WireResponse, DaemonError> {
        let response = self.exchange(Command::GetOutcome {
            target_client_id: client_id,
            target_request_id: target,
        })?;
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
        let request = Self::request(Command::Subscribe {
            events: events.to_vec(),
        });
        stream
            .write_all(&encode_request(&request)?)
            .map_err(map_io_error)?;
        let mut reader = BufReader::new(stream);
        let response = parse_reply(
            &read_frame(&mut reader, &mut Vec::new())?,
            request.request_id,
        )?;
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
            // Survives read timeouts so a frame split across polls is kept.
            let mut partial = Vec::new();
            loop {
                if cancel_rx.try_recv().is_ok() {
                    break;
                }
                let line = match read_frame(&mut reader, &mut partial) {
                    Ok(line) => line,
                    Err(DaemonError::Timeout) => continue,
                    Err(_) => break,
                };
                // A malformed event frame ends the stream so the subscriber
                // sees a disconnect instead of silently missing events.
                let Some(event) = serde_json::from_str::<WireResponse>(&line)
                    .ok()
                    .filter(|response| response.version == PROTOCOL_VERSION && response.ok)
                    .and_then(|response| response.event)
                else {
                    break;
                };
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

fn encode_request(value: &WireRequest) -> Result<Vec<u8>, DaemonError> {
    let body =
        serde_json::to_vec(value).map_err(|error| DaemonError::Protocol(error.to_string()))?;
    if body.len() + 1 > MAX_FRAME_BYTES {
        return Err(DaemonError::Protocol(format!(
            "outgoing frame exceeds {MAX_FRAME_BYTES} bytes"
        )));
    }
    let mut frame = body;
    frame.push(b'\n');
    Ok(frame)
}

/// Reads one frame. On error, `bytes` keeps what was read so a caller polling
/// with a read timeout can resume the same frame.
fn read_frame(reader: &mut impl BufRead, bytes: &mut Vec<u8>) -> Result<String, DaemonError> {
    let limit = (MAX_FRAME_BYTES + 1).saturating_sub(bytes.len());
    reader
        .take(limit as u64)
        .read_until(b'\n', bytes)
        .map_err(map_io_error)?;
    let mut frame = std::mem::take(bytes);
    if frame.is_empty() {
        return Err(DaemonError::Protocol("no response from daemon".into()));
    }
    if frame.len() > MAX_FRAME_BYTES {
        return Err(DaemonError::Protocol(format!(
            "incoming frame exceeds {MAX_FRAME_BYTES} bytes"
        )));
    }
    if frame.pop() != Some(b'\n') {
        return Err(DaemonError::Protocol(
            "daemon closed the connection mid-frame".into(),
        ));
    }
    String::from_utf8(frame).map_err(|error| DaemonError::Protocol(error.to_string()))
}

/// Decodes the reply to `request_id`. The daemon answers a request it could not
/// parse or admit without a request ID, so that error wins over correlation.
fn parse_reply(line: &str, request_id: RequestId) -> Result<WireResponse, DaemonError> {
    let response: WireResponse = serde_json::from_str(line)
        .map_err(|error| DaemonError::Protocol(format!("invalid response JSON: {error}")))?;
    if response.version != PROTOCOL_VERSION {
        return Err(DaemonError::Protocol(format!(
            "unsupported response version {}; expected {PROTOCOL_VERSION}",
            response.version
        )));
    }
    if !response.ok && response.request_id.is_none() {
        return Err(map_wire_error(response));
    }
    if response.request_id != Some(request_id) {
        return Err(DaemonError::Protocol(format!(
            "response request ID {:?} does not match {}",
            response.request_id,
            request_id.get()
        )));
    }
    Ok(response)
}

fn map_wire_error(response: WireResponse) -> DaemonError {
    let Some(error) = response.error else {
        return DaemonError::Protocol("daemon returned failure without an error".into());
    };
    match error.code {
        ErrorCode::NotRunning => DaemonError::NotRunning,
        ErrorCode::PermissionDenied => DaemonError::PermissionDenied,
        // Expired in the queue: it never started, so keep the daemon's
        // message and report it as a plain (retry-safe) rejection.
        ErrorCode::Timeout if response.outcome == RequestOutcome::Expired => {
            DaemonError::Rejected(error.message)
        }
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
    use std::io::Cursor;
    use std::os::unix::net::UnixListener;
    use z13helper_core::error::WireError;

    /// Serves one connection: reads the request line, then hands over.
    fn fake_daemon(
        name: &str,
        serve: impl FnOnce(UnixStream, WireRequest) + Send + 'static,
    ) -> Client {
        let directory = std::env::temp_dir().join(format!(
            "z13helper-client-{name}-{}-{}",
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
            let _ = std::fs::remove_dir_all(&directory);
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            serve(stream, serde_json::from_str(line.trim()).unwrap());
        });
        Client::with_path(path)
    }

    fn reply(stream: &mut UnixStream, response: &WireResponse) {
        writeln!(stream, "{}", serde_json::to_string(response).unwrap()).unwrap();
    }

    #[test]
    fn missing_socket_error_is_not_running() {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(classify_dial_error(error), DaemonError::NotRunning);
    }

    #[test]
    fn outcome_lookup_returns_progress_with_target_correlation() {
        let target_client_id = ClientId::new(9).unwrap();
        let target = RequestId::new(77).unwrap();
        let client = fake_daemon("lookup", move |mut stream, request| {
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
            reply(&mut stream, &response);
        });
        let response = client.get_outcome_for(target_client_id, target).unwrap();
        assert_eq!(response.outcome, RequestOutcome::Started);
        assert_eq!(response.outcome_client_id, Some(target_client_id));
        assert_eq!(response.outcome_request_id, Some(target));
    }

    #[test]
    fn failed_outcome_lookup_is_retryable_without_an_uncacheable_recovery_token() {
        let client = fake_daemon("lookup-close", |_, _| {});
        let error = client
            .get_outcome_for(ClientId::new(9).unwrap(), RequestId::new(77).unwrap())
            .unwrap_err();
        assert!(!matches!(error, DaemonError::OutcomeUnknown { .. }));
    }

    #[test]
    fn frames_are_bounded_in_both_directions() {
        let mut exact = vec![b'x'; MAX_FRAME_BYTES - 1];
        exact.push(b'\n');
        let frame = read_frame(&mut BufReader::new(Cursor::new(exact)), &mut Vec::new()).unwrap();
        assert_eq!(frame.len(), MAX_FRAME_BYTES - 1);

        let mut reader = BufReader::new(Cursor::new(vec![b'x'; MAX_FRAME_BYTES + 1]));
        assert!(matches!(
            read_frame(&mut reader, &mut Vec::new()),
            Err(DaemonError::Protocol(message)) if message.contains("exceeds")
        ));

        let request = WireRequest {
            version: PROTOCOL_VERSION,
            client_id: ClientId::new(1).unwrap(),
            request_id: RequestId::new(99).unwrap(),
            command: Command::GetFactoryFanCurves {
                ppd_profiles: vec!["x".repeat(MAX_FRAME_BYTES)],
            },
        };
        assert!(matches!(
            encode_request(&request),
            Err(DaemonError::Protocol(message)) if message.contains("exceeds")
        ));
    }

    #[test]
    fn get_state_uses_versioned_protocol() {
        let client = fake_daemon("get-state", |mut stream, request| {
            let wire = serde_json::to_string(&request).unwrap();
            assert!(wire.contains("\"version\":3"));
            assert!(wire.contains("\"cmd\":\"get-state\""));
            let mut response = WireResponse::success(request.request_id);
            response.state = Some(WireStatus {
                ppd_profile: Some("balanced".into()),
                ..WireStatus::default()
            });
            reply(&mut stream, &response);
        });
        let state = client.get_state().unwrap();
        assert_eq!(state.ppd_profile.as_deref(), Some("balanced"));
    }

    #[test]
    fn factory_fan_curves_are_returned_by_ppd_profile() {
        let expected = z13helper_core::stock_fan_curves(Some("balanced"));
        let client = fake_daemon("factory-fans", move |mut stream, request| {
            let wire = serde_json::to_string(&request).unwrap();
            assert!(wire.contains("\"cmd\":\"get-factory-fan-curves\""));
            assert!(wire.contains("\"balanced\""));
            let mut response = WireResponse::success(request.request_id);
            response.factory_fan_curves = Some(HashMap::from([("balanced".into(), expected)]));
            reply(&mut stream, &response);
        });
        let curves = client.factory_fan_curves(vec!["balanced".into()]).unwrap();
        assert_eq!(curves["balanced"], expected);
    }

    #[test]
    fn a_reply_cut_off_after_send_leaves_the_outcome_unknown() {
        let client = fake_daemon("cut-off", |mut stream, _| {
            stream.write_all(b"{\"version\":3,").unwrap();
        });
        assert!(matches!(
            client.get_state(),
            Err(DaemonError::OutcomeUnknown { .. })
        ));
    }

    #[test]
    fn a_frame_cut_off_at_eof_is_not_reported_as_oversized() {
        let mut reader = BufReader::new(Cursor::new(b"{\"version\":3,".to_vec()));
        assert!(matches!(
            read_frame(&mut reader, &mut Vec::new()),
            Err(DaemonError::Protocol(message)) if message.contains("closed")
        ));
    }

    #[test]
    fn an_expired_request_keeps_the_daemon_message() {
        let mut response = WireResponse::success(RequestId::new(5).unwrap());
        response.ok = false;
        response.outcome = RequestOutcome::Expired;
        response.error = Some(WireError {
            code: ErrorCode::Timeout,
            message: "request expired before start".into(),
        });
        assert_eq!(
            map_wire_error(response),
            DaemonError::Rejected("request expired before start".into())
        );
    }

    #[test]
    fn an_uncorrelated_daemon_error_is_reported_as_the_daemon_error() {
        fn reject(mut stream: UnixStream, _: WireRequest) {
            let mut response = WireResponse::success(RequestId::new(1).unwrap());
            response.request_id = None;
            response.ok = false;
            response.error = Some(WireError {
                code: ErrorCode::Rejected,
                message: "too many active clients".into(),
            });
            reply(&mut stream, &response);
        }
        let expected = DaemonError::Rejected("too many active clients".into());
        assert_eq!(
            fake_daemon("uncorrelated", reject).get_state(),
            Err(expected.clone())
        );
        assert_eq!(
            fake_daemon("uncorrelated-subscribe", reject)
                .subscribe(&[EventTopic::StateChanged])
                .err(),
            Some(expected)
        );
    }

    #[test]
    fn subscription_keeps_partial_frames_and_ends_on_malformed_ones() {
        let client = fake_daemon("subscribe-partial", |mut stream, request| {
            reply(&mut stream, &WireResponse::success(request.request_id));
            let mut event = WireResponse::success(request.request_id);
            event.request_id = None;
            event.event = Some(DaemonEvent {
                kind: EventTopic::GuiToggle,
                action: None,
                on_battery: None,
            });
            let frame = format!("{}\n", serde_json::to_string(&event).unwrap());
            let (head, tail) = frame.split_at(10);
            stream.write_all(head.as_bytes()).unwrap();
            // Outlast the client's 500 ms poll so the frame spans a read timeout.
            std::thread::sleep(Duration::from_millis(700));
            stream.write_all(tail.as_bytes()).unwrap();
            stream.write_all(b"not json\n").unwrap();
            // Hold the connection open until the client hangs up.
            let _ = stream.read(&mut [0_u8; 1]);
        });
        let (events, _cancel) = client.subscribe(&[EventTopic::GuiToggle]).unwrap();
        let event = events.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(event.kind, EventTopic::GuiToggle);
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(3)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}
