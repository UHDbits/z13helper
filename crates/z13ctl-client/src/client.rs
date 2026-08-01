//! Newline-delimited JSON Unix socket client for the z13ctl daemon.
//!
//! One request per connection (except `subscribe`, which streams). Dial
//! failures collapse to [`DaemonError::NotRunning`] or
//! [`DaemonError::PermissionDenied`] — never silent success.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::DaemonError;
use crate::types::{Request, Response, State};

const DIAL_TIMEOUT: Duration = Duration::from_secs(1);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Client for the z13ctl daemon socket.
#[derive(Debug, Clone)]
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
        Self {
            socket_path: PathBuf::from(Self::socket_path()),
        }
    }

    /// Override the socket path (useful in tests).
    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: path.into(),
        }
    }

    /// Resolve the daemon socket path the same way the Go client does.
    pub fn socket_path() -> String {
        let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        format!("{runtime}/z13ctl/z13ctl.sock")
    }

    fn dial(&self) -> Result<UnixStream, DaemonError> {
        // AF_UNIX connect is effectively instant (ENOENT / ECONNREFUSED /
        // success). The Go client's 1s dialTimeout is for TCP-style sockets;
        // we still apply the 10s exchange deadline after connect.
        let _ = DIAL_TIMEOUT;
        match UnixStream::connect(&self.socket_path) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(COMMAND_TIMEOUT))
                    .map_err(|e| DaemonError::Protocol(e.to_string()))?;
                stream
                    .set_write_timeout(Some(COMMAND_TIMEOUT))
                    .map_err(|e| DaemonError::Protocol(e.to_string()))?;
                Ok(stream)
            }
            Err(e) => Err(classify_dial_error(e)),
        }
    }

    fn exchange(&self, req: &Request) -> Result<Response, DaemonError> {
        let mut stream = self.dial()?;
        let body = serde_json::to_string(req).map_err(|e| DaemonError::Protocol(e.to_string()))?;
        stream
            .write_all(body.as_bytes())
            .and_then(|_| stream.write_all(b"\n"))
            .map_err(map_io_error)?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return Err(DaemonError::Protocol("no response from daemon".into())),
            Ok(_) => {}
            Err(e) => return Err(map_io_error(e)),
        }
        let resp: Response = serde_json::from_str(line.trim())
            .map_err(|e| DaemonError::Protocol(format!("invalid response JSON: {e}")))?;
        if !resp.ok {
            return Err(DaemonError::Rejected(
                resp.error.unwrap_or_else(|| "unknown error".into()),
            ));
        }
        Ok(resp)
    }

    fn send_ok(&self, req: Request) -> Result<(), DaemonError> {
        self.exchange(&req)?;
        Ok(())
    }

    fn send_value(&self, req: Request) -> Result<String, DaemonError> {
        let resp = self.exchange(&req)?;
        resp.value
            .ok_or_else(|| DaemonError::Protocol("missing value in response".into()))
    }

    // --- Commands ---

    pub fn get_state(&self) -> Result<State, DaemonError> {
        let resp = self.exchange(&Request {
            cmd: "get-state".into(),
            ..Default::default()
        })?;
        resp.state
            .ok_or_else(|| DaemonError::Protocol("missing state in get-state response".into()))
    }

    pub fn profile_set(&self, profile: &str) -> Result<(), DaemonError> {
        self.send_ok(Request {
            cmd: "profile".into(),
            set: Some(profile.into()),
            ..Default::default()
        })
    }

    /// Read the stock base from sysfs. Never returns `"custom"`.
    /// Normalizes `"low-power"` → `"quiet"`.
    pub fn profile_get(&self) -> Result<String, DaemonError> {
        let raw = self.send_value(Request {
            cmd: "profile-get".into(),
            ..Default::default()
        })?;
        Ok(normalize_profile(&raw))
    }

    pub fn tdp_set(
        &self,
        set: u32,
        pl1: Option<u32>,
        pl2: Option<u32>,
        pl3: Option<u32>,
        force: bool,
    ) -> Result<(), DaemonError> {
        self.send_ok(Request {
            cmd: "tdp".into(),
            set: Some(set.to_string()),
            pl1: pl1.map(|v| v.to_string()),
            pl2: pl2.map(|v| v.to_string()),
            pl3: pl3.map(|v| v.to_string()),
            force: if force { Some(true) } else { None },
            ..Default::default()
        })
    }

    pub fn tdp_reset(&self) -> Result<(), DaemonError> {
        self.send_ok(Request {
            cmd: "tdp-reset".into(),
            ..Default::default()
        })
    }

    /// Set fan curve from 8 `[temp, pwm]` pairs.
    pub fn fan_curve_set(&self, points: &[[i32; 2]; 8]) -> Result<(), DaemonError> {
        let curve = points
            .iter()
            .map(|p| format!("{}:{}", p[0], p[1]))
            .collect::<Vec<_>>()
            .join(",");
        self.send_ok(Request {
            cmd: "fancurve".into(),
            set: Some(curve),
            ..Default::default()
        })
    }

    pub fn fan_curve_reset(&self) -> Result<(), DaemonError> {
        self.send_ok(Request {
            cmd: "fancurve-reset".into(),
            ..Default::default()
        })
    }

    pub fn undervolt_set(&self, cpu_co: i32) -> Result<(), DaemonError> {
        self.send_ok(Request {
            cmd: "undervolt".into(),
            set: Some(cpu_co.to_string()),
            ..Default::default()
        })
    }

    pub fn undervolt_reset(&self) -> Result<(), DaemonError> {
        self.send_ok(Request {
            cmd: "undervolt-reset".into(),
            ..Default::default()
        })
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
        self.send_ok(Request {
            cmd: "apply".into(),
            mode: Some(mode.into()),
            color: Some(color.into()),
            color2: Some(color2.into()),
            speed: Some(speed.into()),
            brightness: Some(brightness),
            device: if device.is_empty() {
                None
            } else {
                Some(device.into())
            },
            ..Default::default()
        })
    }

    pub fn lighting_off(&self, device: &str) -> Result<(), DaemonError> {
        self.send_ok(Request {
            cmd: "off".into(),
            device: if device.is_empty() {
                None
            } else {
                Some(device.into())
            },
            ..Default::default()
        })
    }

    pub fn brightness_set(&self, brightness: i32, device: &str) -> Result<(), DaemonError> {
        self.send_ok(Request {
            cmd: "brightness".into(),
            brightness: Some(brightness),
            device: if device.is_empty() {
                None
            } else {
                Some(device.into())
            },
            ..Default::default()
        })
    }

    pub fn battery_limit_set(&self, limit: i32) -> Result<(), DaemonError> {
        self.send_ok(Request {
            cmd: "batterylimit".into(),
            set: Some(limit.to_string()),
            ..Default::default()
        })
    }

    pub fn battery_limit_get(&self) -> Result<i32, DaemonError> {
        let v = self.send_value(Request {
            cmd: "batterylimit-get".into(),
            ..Default::default()
        })?;
        v.parse()
            .map_err(|e| DaemonError::Protocol(format!("invalid battery limit: {e}")))
    }

    pub fn panel_overdrive_set(&self, value: i32) -> Result<(), DaemonError> {
        self.send_ok(Request {
            cmd: "paneloverdrive".into(),
            set: Some(value.to_string()),
            ..Default::default()
        })
    }

    pub fn panel_overdrive_get(&self) -> Result<i32, DaemonError> {
        let v = self.send_value(Request {
            cmd: "paneloverdrive-get".into(),
            ..Default::default()
        })?;
        v.parse()
            .map_err(|e| DaemonError::Protocol(format!("invalid panel overdrive: {e}")))
    }

    pub fn boot_sound_set(&self, value: i32) -> Result<(), DaemonError> {
        self.send_ok(Request {
            cmd: "bootsound".into(),
            set: Some(value.to_string()),
            ..Default::default()
        })
    }

    pub fn boot_sound_get(&self) -> Result<i32, DaemonError> {
        let v = self.send_value(Request {
            cmd: "bootsound-get".into(),
            ..Default::default()
        })?;
        v.parse()
            .map_err(|e| DaemonError::Protocol(format!("invalid boot sound: {e}")))
    }

    /// Open a long-lived subscribe connection. Returns a channel of event
    /// names (currently only `"gui-toggle"`) and a cancel handle.
    pub fn subscribe(
        &self,
        events: &[&str],
    ) -> Result<(std::sync::mpsc::Receiver<String>, SubscribeCancel), DaemonError> {
        let mut stream = self.dial()?;
        let req = Request {
            cmd: "subscribe".into(),
            events: Some(events.iter().map(|s| (*s).to_string()).collect()),
            ..Default::default()
        };
        let body = serde_json::to_string(&req).map_err(|e| DaemonError::Protocol(e.to_string()))?;
        stream
            .write_all(body.as_bytes())
            .and_then(|_| stream.write_all(b"\n"))
            .map_err(map_io_error)?;

        // Read ack without a BufReader so we don't swallow subsequent events.
        let ack_line = read_line_raw(&mut stream)?;
        let ack: Response = serde_json::from_str(ack_line.trim())
            .map_err(|e| DaemonError::Protocol(format!("invalid subscribe ack: {e}")))?;
        if !ack.ok {
            return Err(DaemonError::Rejected(
                ack.error.unwrap_or_else(|| "subscribe failed".into()),
            ));
        }

        // Poll with a short timeout so cancel is responsive.
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(|e| DaemonError::Protocol(e.to_string()))?;

        let (tx, rx) = std::sync::mpsc::channel();
        let (cancel_tx, cancel_rx) = std::sync::mpsc::channel::<()>();
        let cancel = SubscribeCancel { tx: cancel_tx };

        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            loop {
                if cancel_rx.try_recv().is_ok() {
                    break;
                }
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(ev) = serde_json::from_str::<Response>(line.trim()) {
                            if ev.ok {
                                if let Some(name) = ev.event {
                                    if !name.is_empty() && tx.send(name).is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(_) => break,
                }
            }
        });

        Ok((rx, cancel))
    }
}

/// Drop or call [`SubscribeCancel::cancel`] to close the subscribe connection.
pub struct SubscribeCancel {
    tx: std::sync::mpsc::Sender<()>,
}

impl SubscribeCancel {
    pub fn cancel(self) {
        let _ = self.tx.send(());
    }
}

fn normalize_profile(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "low-power" => "quiet".into(),
        other => other.to_string(),
    }
}

fn classify_dial_error(e: std::io::Error) -> DaemonError {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => DaemonError::PermissionDenied,
        std::io::ErrorKind::TimedOut => DaemonError::NotRunning,
        std::io::ErrorKind::NotFound
        | std::io::ErrorKind::ConnectionRefused
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::ConnectionReset => DaemonError::NotRunning,
        _ => {
            // Go client treats ALL dial failures as (handled=false).
            // Mirror that, but keep PermissionDenied distinct when we see it.
            let msg = e.to_string().to_ascii_lowercase();
            if msg.contains("permission") || msg.contains("eacces") {
                DaemonError::PermissionDenied
            } else {
                DaemonError::NotRunning
            }
        }
    }
}

fn map_io_error(e: std::io::Error) -> DaemonError {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => DaemonError::Timeout,
        std::io::ErrorKind::PermissionDenied => DaemonError::PermissionDenied,
        _ => DaemonError::Protocol(e.to_string()),
    }
}

fn read_line_raw(stream: &mut UnixStream) -> Result<String, DaemonError> {
    use std::io::Read;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(DaemonError::Protocol("no response from daemon".into()));
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
            }
            Err(e) => return Err(map_io_error(e)),
        }
    }
    String::from_utf8(buf).map_err(|e| DaemonError::Protocol(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use std::thread;

    fn with_fake_server<F>(handler: F) -> Client
    where
        F: Fn(String) -> String + Send + Sync + 'static,
    {
        let dir = std::env::temp_dir().join(format!(
            "z13ctl-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("z13ctl.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let handler = Arc::new(handler);
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1];
                while stream.read(&mut tmp).unwrap_or(0) > 0 {
                    if tmp[0] == b'\n' {
                        break;
                    }
                    buf.push(tmp[0]);
                }
                let req = String::from_utf8_lossy(&buf).to_string();
                let resp = handler(req);
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(b"\n");
            }
        });
        // Brief settle so the listener is ready.
        thread::sleep(Duration::from_millis(20));
        Client::with_path(sock)
    }

    #[test]
    fn get_state_parses_omitempty() {
        let client = with_fake_server(|_| {
            r#"{"ok":true,"state":{"lighting":{"enabled":true,"mode":"static","color":"FF0000","color2":"000000","speed":"normal","brightness":3},"undervolt_available":true}}"#.into()
        });
        let state = client.get_state().unwrap();
        assert!(state.undervolt_available);
        assert!(state.temperature.is_none());
        assert!(state.battery_limit.is_none());
        assert_eq!(state.lighting.mode, "static");
    }

    #[test]
    fn profile_get_normalizes_low_power() {
        let client = with_fake_server(|_| r#"{"ok":true,"value":"low-power"}"#.into());
        assert_eq!(client.profile_get().unwrap(), "quiet");
    }

    #[test]
    fn rejected_maps_to_error() {
        let client = with_fake_server(|_| {
            r#"{"ok":false,"error":"PL1 80W exceeds safe sustained max (75W); use force flag"}"#
                .into()
        });
        match client.profile_set("balanced") {
            Err(DaemonError::Rejected(msg)) => {
                assert!(msg.contains("force flag"));
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn missing_socket_is_not_running() {
        let client = Client::with_path("/tmp/z13ctl-definitely-missing-socket.sock");
        assert_eq!(client.get_state().unwrap_err(), DaemonError::NotRunning);
    }

    #[test]
    fn tdp_set_includes_mandatory_set_field() {
        let client = with_fake_server(|req| {
            assert!(req.contains(r#""cmd":"tdp""#));
            assert!(req.contains(r#""set":"60""#));
            assert!(req.contains(r#""pl1":"55""#));
            assert!(req.contains(r#""force":true"#));
            r#"{"ok":true}"#.into()
        });
        client
            .tdp_set(60, Some(55), Some(65), Some(70), true)
            .unwrap();
    }

    #[test]
    fn fan_curve_formats_wire_string() {
        let client = with_fake_server(|req| {
            assert!(req.contains("48:2,53:22"));
            r#"{"ok":true}"#.into()
        });
        let curve = [
            [48, 2],
            [53, 22],
            [57, 30],
            [60, 43],
            [63, 56],
            [65, 68],
            [70, 89],
            [76, 102],
        ];
        client.fan_curve_set(&curve).unwrap();
    }
}
