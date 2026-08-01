//! Client for the privileged z13-helper direct fan companion.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::DaemonError;

const DEFAULT_SOCKET: &str = "/run/z13-helper/fan.sock";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct ManualFanClient {
    socket_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManualFanPoint {
    pub temp: i32,
    pub pwm: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ManualFanStatus {
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub target_pwm: Option<i32>,
    #[serde(default)]
    pub fan_rpm: Vec<i32>,
    #[serde(default)]
    pub temperature: Option<i32>,
    #[serde(default)]
    pub fail_safe: Option<String>,
}

#[derive(Debug, Serialize)]
struct ManualFanRequest {
    cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    points: Option<Vec<ManualFanPoint>>,
}

#[derive(Debug, Deserialize)]
struct ManualFanResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    status: Option<ManualFanStatus>,
}

impl Default for ManualFanClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ManualFanClient {
    pub fn new() -> Self {
        Self::with_path(DEFAULT_SOCKET)
    }

    pub fn with_path(path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: path.into(),
        }
    }

    /// Human-readable status for companion dial/command failures.
    ///
    /// [`DaemonError`]'s `Display` is worded for z13ctl; callers probing the
    /// optional fan companion should use this instead.
    pub fn describe_error(error: &DaemonError) -> String {
        match error {
            DaemonError::NotRunning => {
                "companion not running — sudo make install-fan-service".into()
            }
            DaemonError::PermissionDenied => {
                "permission denied on fan socket — join the z13-helper group and re-login".into()
            }
            DaemonError::Timeout => "fan companion timed out".into(),
            DaemonError::Rejected(message) | DaemonError::Protocol(message) => message.clone(),
        }
    }

    pub fn probe(&self) -> Result<ManualFanStatus, DaemonError> {
        self.status_command("probe")
    }

    pub fn status(&self) -> Result<ManualFanStatus, DaemonError> {
        self.status_command("status")
    }

    pub fn enable(&self, curve: &[[i32; 2]; 8]) -> Result<ManualFanStatus, DaemonError> {
        let points = curve
            .iter()
            .map(|point| ManualFanPoint {
                temp: point[0],
                pwm: point[1],
            })
            .collect();
        self.exchange(ManualFanRequest {
            cmd: "enable".into(),
            points: Some(points),
        })?
        .status
        .ok_or_else(|| DaemonError::Protocol("missing companion status".into()))
    }

    pub fn release(&self) -> Result<(), DaemonError> {
        self.exchange(ManualFanRequest {
            cmd: "release".into(),
            points: None,
        })?;
        Ok(())
    }

    fn status_command(&self, cmd: &str) -> Result<ManualFanStatus, DaemonError> {
        self.exchange(ManualFanRequest {
            cmd: cmd.into(),
            points: None,
        })?
        .status
        .ok_or_else(|| DaemonError::Protocol("missing companion status".into()))
    }

    fn exchange(&self, request: ManualFanRequest) -> Result<ManualFanResponse, DaemonError> {
        let mut stream = UnixStream::connect(&self.socket_path).map_err(classify_dial_error)?;
        stream
            .set_read_timeout(Some(COMMAND_TIMEOUT))
            .and_then(|_| stream.set_write_timeout(Some(COMMAND_TIMEOUT)))
            .map_err(map_io_error)?;
        let body = serde_json::to_vec(&request)
            .map_err(|error| DaemonError::Protocol(error.to_string()))?;
        stream.write_all(&body).map_err(map_io_error)?;
        stream.write_all(b"\n").map_err(map_io_error)?;

        let mut line = String::new();
        match BufReader::new(stream).read_line(&mut line) {
            Ok(0) => return Err(DaemonError::Protocol("no response from companion".into())),
            Ok(_) => {}
            Err(error) => return Err(map_io_error(error)),
        }
        let response: ManualFanResponse = serde_json::from_str(line.trim()).map_err(|error| {
            DaemonError::Protocol(format!("invalid companion response: {error}"))
        })?;
        if response.ok {
            Ok(response)
        } else {
            Err(DaemonError::Rejected(
                response
                    .error
                    .unwrap_or_else(|| "manual fan command rejected".into()),
            ))
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
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn fake(response: &'static str) -> (ManualFanClient, thread::JoinHandle<String>) {
        let path = std::env::temp_dir().join(format!(
            "z13-helper-fan-client-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut byte = [0];
                if stream.read(&mut byte).unwrap() == 0 || byte[0] == b'\n' {
                    break;
                }
                bytes.push(byte[0]);
            }
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(b"\n").unwrap();
            String::from_utf8(bytes).unwrap()
        });
        (ManualFanClient::with_path(path), handle)
    }

    #[test]
    fn enable_sends_all_curve_points() {
        let (client, handle) = fake(
            r#"{"ok":true,"status":{"available":true,"active":true,"target_pwm":204,"fan_rpm":[3000,3100]}}"#,
        );
        let curve = [
            [30, 20],
            [40, 40],
            [50, 60],
            [60, 80],
            [70, 100],
            [80, 140],
            [90, 200],
            [100, 255],
        ];
        assert!(client.enable(&curve).unwrap().active);
        let request = handle.join().unwrap();
        let value: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(value["cmd"], "enable");
        assert_eq!(value["points"].as_array().unwrap().len(), 8);
    }

    #[test]
    fn probe_parses_unavailable_status() {
        let (client, handle) =
            fake(r#"{"ok":true,"status":{"available":false,"fail_safe":"unsupported model"}}"#);
        let status = client.probe().unwrap();
        assert!(!status.available);
        assert_eq!(status.fail_safe.as_deref(), Some("unsupported model"));
        handle.join().unwrap();
    }

    #[test]
    fn describe_error_mentions_companion_not_z13ctl() {
        let message = ManualFanClient::describe_error(&DaemonError::NotRunning);
        assert!(message.contains("companion"));
        assert!(!message.contains("z13ctl"));
    }
}
