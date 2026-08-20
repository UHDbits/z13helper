use z13helper_core::error::{DaemonError, ErrorCode, WireError};
use z13helper_core::protocol::{Command, PROTOCOL_VERSION, WireRequest, WireResponse};

use crate::backend::Backend;

pub const MAX_SUBSCRIPTION_EVENTS: usize = 4;

pub enum Dispatch {
    Reply(Box<WireResponse>),
    Subscribe(Vec<String>),
    ControllerCapture(bool),
}

pub fn handle_line(backend: &mut Backend, line: &str) -> Dispatch {
    let request: WireRequest = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(error) => {
            return Dispatch::Reply(Box::new(failure(ErrorCode::Protocol, error.to_string())));
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
        return match validate_subscription_events(&events) {
            Ok(()) => Dispatch::Subscribe(events),
            Err(message) => Dispatch::Reply(Box::new(failure(ErrorCode::Rejected, message))),
        };
    }
    if let Command::SetControllerCapture { enabled } = request.command {
        return Dispatch::ControllerCapture(enabled);
    }
    Dispatch::Reply(Box::new(match dispatch(backend, request.command) {
        Ok(response) => response,
        Err(error) => from_error(error),
    }))
}

fn validate_subscription_events(events: &[String]) -> Result<(), String> {
    if events.is_empty() {
        return Err("at least one subscription event is required".into());
    }
    if events.len() > MAX_SUBSCRIPTION_EVENTS {
        return Err(format!(
            "at most {MAX_SUBSCRIPTION_EVENTS} subscription events are allowed"
        ));
    }
    if let Some(event) = events.iter().find(|event| {
        !matches!(
            event.as_str(),
            "state-changed" | "gui-toggle" | "controller-action" | "power-source-changed"
        )
    }) {
        return Err(format!("unknown subscription event {event:?}"));
    }
    Ok(())
}

fn dispatch(backend: &mut Backend, command: Command) -> Result<WireResponse, DaemonError> {
    let mut response = WireResponse::success();
    match command {
        Command::GetState => response.state = Some(backend.state()),
        Command::Probe => response.probe = Some(backend.probe()),
        Command::GetFactoryFanCurves { ppd_profiles } => {
            response.factory_fan_curves = Some(backend.factory_fan_curves(ppd_profiles)?)
        }
        Command::Apply { request } => response.apply = Some(backend.apply(request)?),
        Command::ApplyUndervoltOnce { offset } => backend.apply_undervolt_once(offset)?,
        Command::SetBatteryLimit { limit } => backend.set_battery_limit(limit)?,
        Command::SetBatteryOneTimeCharge { enabled } => {
            backend.set_battery_one_time_charge(enabled)?
        }
        Command::SetPanelOverdrive { enabled } => backend.set_panel_overdrive(enabled)?,
        Command::SetLighting { device, state } => backend.set_lighting(device, state)?,
        Command::ReleaseFans => backend.release_fans()?,
        Command::SetControllerCapture { .. } => unreachable!(),
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
        factory_fan_curves: None,
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
    fn subscription_events_are_known_and_bounded() {
        assert!(validate_subscription_events(&[]).is_err());
        assert!(validate_subscription_events(&["state-changed".into()]).is_ok());
        assert!(validate_subscription_events(&["not-an-event".into()]).is_err());
        assert!(
            validate_subscription_events(
                &(0..=MAX_SUBSCRIPTION_EVENTS)
                    .map(|_| "state-changed".to_owned())
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
    }

    #[test]
    fn malformed_request_returns_protocol_error() {
        let response = failure(ErrorCode::Protocol, "bad request");
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "protocol");
    }
}
