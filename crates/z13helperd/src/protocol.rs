use z13helper_core::error::{DaemonError, ErrorCode, WireError};
use z13helper_core::protocol::{Command, WireRequest, WireResponse, PROTOCOL_VERSION};

use crate::backend::Backend;

pub enum Dispatch {
    Reply(Box<WireResponse>),
    Subscribe(Vec<String>),
}

pub fn handle_line(backend: &mut Backend, line: &str) -> Dispatch {
    let request: WireRequest = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(error) => {
            return Dispatch::Reply(Box::new(failure(ErrorCode::Protocol, error.to_string())))
        }
    };
    if request.version != PROTOCOL_VERSION {
        return Dispatch::Reply(Box::new(failure(
            ErrorCode::Protocol,
            format!(
                "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                request.version
            ),
        )));
    }
    if let Command::Subscribe { events } = request.command {
        return Dispatch::Subscribe(events);
    }
    Dispatch::Reply(Box::new(match dispatch(backend, request.command) {
        Ok(response) => response,
        Err(error) => from_error(error),
    }))
}

fn dispatch(backend: &mut Backend, command: Command) -> Result<WireResponse, DaemonError> {
    let mut response = WireResponse::success();
    match command {
        Command::GetState => response.state = Some(backend.state()),
        Command::Probe => response.probe = Some(backend.probe()),
        Command::Apply { request } => response.apply = Some(backend.apply(request)?),
        Command::SetBatteryLimit { limit } => backend.set_battery_limit(limit)?,
        Command::SetPanelOverdrive { enabled } => backend.set_panel_overdrive(enabled)?,
        Command::SetBootSound { enabled } => backend.set_boot_sound(enabled)?,
        Command::SetLighting { device, state } => backend.set_lighting(device, state)?,
        Command::ReleaseFans => backend.release_fans()?,
        Command::Subscribe { .. } => unreachable!(),
    }
    Ok(response)
}

fn from_error(error: DaemonError) -> WireResponse {
    match error {
        DaemonError::NotRunning => failure(ErrorCode::NotRunning, "daemon is not running"),
        DaemonError::PermissionDenied => failure(ErrorCode::PermissionDenied, error.to_string()),
        DaemonError::Timeout => failure(ErrorCode::Timeout, error.to_string()),
        DaemonError::Rejected(message) => failure(ErrorCode::Rejected, message),
        DaemonError::Protocol(message) => failure(ErrorCode::Protocol, message),
    }
}

pub fn failure(code: ErrorCode, message: impl Into<String>) -> WireResponse {
    WireResponse {
        ok: false,
        state: None,
        apply: None,
        probe: None,
        event: None,
        error: Some(WireError {
            code,
            message: message.into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_request_returns_protocol_error() {
        let response = failure(ErrorCode::Protocol, "bad request");
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "protocol");
    }
}
