use z13helper_core::error::{DaemonError, ErrorCode, WireError};
use z13helper_core::protocol::{
    ClientId, Command, DaemonState, EventTopic, PROTOCOL_VERSION, RequestId, RequestOutcome,
    WireCapabilities, WireRequest, WireResponse, WireStatus,
};

pub const MAX_SUBSCRIPTION_EVENTS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Effect {
    StateChanged,
    ControllerCapture(bool),
}

pub struct Reply {
    pub client_id: Option<ClientId>,
    pub response: WireResponse,
    pub effects: Vec<Effect>,
}

/// Convert the persisted schema-v1 snapshot into the strict protocol-v3
/// status DTO. Historical flattened fields and the inferred
/// `high_power_fan_protection` flag deliberately never cross this boundary.
pub fn wire_status(state: &DaemonState) -> WireStatus {
    let mut devices: std::collections::BTreeMap<String, z13helper_core::protocol::LightingState> =
        state
            .devices
            .as_ref()
            .map(|devices| {
                devices
                    .iter()
                    .map(|(device, lighting)| (device.clone(), lighting.clone()))
                    .collect()
            })
            .unwrap_or_default();
    // The schema-v1 snapshot historically stored one fallback lighting value.
    // Materialize it into the canonical per-device v3 map without changing or
    // migrating the persisted representation.
    for device in ["keyboard", "lightbar"] {
        devices
            .entry(device.to_owned())
            .or_insert_with(|| state.lighting.clone());
    }
    WireStatus {
        generation: state.generation,
        profile: state.profile.clone(),
        overrides: state.overrides,
        ppd_profile: state.ppd_profile.clone(),
        devices,
        battery_limit: state.battery_limit,
        battery_one_time_charge: state.battery_one_time_charge,
        battery: state.battery.clone(),
        panel_overdrive: state.panel_overdrive.map(|value| value != 0),
        fan_curves: state.fan_curves,
        fan_control_mode: state.fan_control_mode,
        tdp: state.tdp,
        undervolt: state.undervolt,
        cpu_temp_limit: state.cpu_temp_limit,
        fan_hysteresis: state.fan_hysteresis,
        fan_temperature_average_seconds: state.fan_temperature_average_seconds,
        direct_fan_duties: state.direct_fan_duties,
        disable_high_power_fan_protection: state.disable_high_power_fan_protection,
        capabilities: WireCapabilities {
            ppd_profiles: state.capabilities.ppd_profiles.clone(),
            direct_fans: state.capabilities.direct_fans,
            undervolt: state.undervolt_available,
        },
        telemetry: state.telemetry.clone(),
        health: state.health.clone(),
    }
}

pub enum Dispatch {
    Reply(Box<Reply>),
    Subscribe {
        client_id: ClientId,
        request_id: RequestId,
        events: Vec<EventTopic>,
    },
    OutcomeQuery {
        request_id: RequestId,
        target_client_id: ClientId,
        target: RequestId,
    },
    Command {
        client_id: ClientId,
        request_id: RequestId,
        command: Box<Command>,
        effects: Vec<Effect>,
    },
}

pub fn parse_line(line: &str) -> Dispatch {
    let request = match parse_request(line) {
        Ok(request) => request,
        Err(response) => {
            return Dispatch::Reply(Box::new(Reply {
                client_id: None,
                response: *response,
                effects: vec![],
            }));
        }
    };
    if let Command::Subscribe { events } = &request.command {
        return match validate_subscription_events(events) {
            Ok(topics) => Dispatch::Subscribe {
                client_id: request.client_id,
                request_id: request.request_id,
                events: topics,
            },
            Err(message) => Dispatch::Reply(Box::new(Reply {
                client_id: Some(request.client_id),
                response: failure(Some(request.request_id), ErrorCode::Rejected, message),
                effects: vec![],
            })),
        };
    }
    if let Command::GetOutcome {
        target_client_id,
        target_request_id: target,
    } = request.command
    {
        return Dispatch::OutcomeQuery {
            request_id: request.request_id,
            target_client_id,
            target,
        };
    }
    if let Command::SetControllerCapture { enabled } = &request.command {
        return Dispatch::Reply(Box::new(Reply {
            client_id: Some(request.client_id),
            response: WireResponse::success(request.request_id),
            effects: vec![Effect::ControllerCapture(*enabled)],
        }));
    }
    let request_id = request.request_id;
    Dispatch::Command {
        client_id: request.client_id,
        request_id,
        effects: command_effects(&request.command),
        command: Box::new(request.command),
    }
}

fn parse_request(line: &str) -> Result<WireRequest, Box<WireResponse>> {
    let request: WireRequest = serde_json::from_str(line)
        .map_err(|error| Box::new(failure(None, ErrorCode::Protocol, error.to_string())))?;
    if request.version != PROTOCOL_VERSION {
        return Err(Box::new(failure(
            Some(request.request_id),
            ErrorCode::Protocol,
            format!(
                "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                request.version
            ),
        )));
    }
    Ok(request)
}

fn validate_subscription_events(events: &[EventTopic]) -> Result<Vec<EventTopic>, String> {
    if events.is_empty() {
        return Err("at least one subscription event is required".into());
    }
    if events.len() > MAX_SUBSCRIPTION_EVENTS {
        return Err(format!(
            "at most {MAX_SUBSCRIPTION_EVENTS} subscription events are allowed"
        ));
    }
    Ok(events.to_vec())
}

fn command_effects(command: &Command) -> Vec<Effect> {
    match command {
        Command::GetState | Command::Probe | Command::GetFactoryFanCurves { .. } => vec![],
        Command::Apply { .. }
        | Command::ApplyUndervoltOnce { .. }
        | Command::SetBatteryLimit { .. }
        | Command::SetBatteryOneTimeCharge { .. }
        | Command::SetPanelOverdrive { .. }
        | Command::SetLighting { .. }
        | Command::ReleaseFans => vec![Effect::StateChanged],
        Command::SetControllerCapture { enabled } => vec![Effect::ControllerCapture(*enabled)],
        Command::Subscribe { .. } => vec![],
        Command::GetOutcome { .. } => vec![],
    }
}

pub fn execute(
    backend: &mut crate::backend::Backend,
    request_id: RequestId,
    command: Command,
) -> WireResponse {
    let mut response = WireResponse::success(request_id);
    let result: Result<(), DaemonError> = (|| {
        match command {
            Command::GetState => response.state = Some(wire_status(&backend.state())),
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
            Command::SetControllerCapture { .. } | Command::GetOutcome { .. } => unreachable!(),
            Command::Subscribe { .. } => unreachable!(),
        }
        Ok(())
    })();
    match result {
        Ok(()) => response,
        Err(error) => from_error(request_id, error),
    }
}

fn from_error(request_id: RequestId, error: DaemonError) -> WireResponse {
    match error {
        DaemonError::NotRunning => failure(
            Some(request_id),
            ErrorCode::NotRunning,
            "daemon is not running",
        ),
        DaemonError::PermissionDenied => failure(
            Some(request_id),
            ErrorCode::PermissionDenied,
            error.to_string(),
        ),
        DaemonError::Timeout => failure(Some(request_id), ErrorCode::Timeout, error.to_string()),
        DaemonError::OutcomeUnknown { request_id, .. } => failure(
            RequestId::new(request_id),
            ErrorCode::Timeout,
            "request outcome is unknown",
        ),
        DaemonError::Rejected(message) => failure(Some(request_id), ErrorCode::Rejected, message),
        DaemonError::Protocol(message) => failure(Some(request_id), ErrorCode::Protocol, message),
    }
}

pub fn failure(
    request_id: Option<RequestId>,
    code: ErrorCode,
    message: impl Into<String>,
) -> WireResponse {
    WireResponse {
        version: PROTOCOL_VERSION,
        request_id,
        outcome: RequestOutcome::Completed,
        outcome_client_id: None,
        outcome_request_id: None,
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
    use z13helper_core::profile::Profile;
    use z13helper_core::protocol::{ApplyRequest, DaemonState, Health, LightingState, Telemetry};

    #[test]
    fn v3_status_golden_has_one_canonical_shape() {
        let state = wire_status(&DaemonState::default());
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            r#"{"generation":0,"profile":"balanced","overrides":{"power":false,"fans":false,"undervolt":false},"ppd_profile":"balanced","devices":{"keyboard":{"enabled":true,"mode":"static","color":"FF0000","color2":"000000","speed":"normal","brightness":3},"lightbar":{"enabled":true,"mode":"static","color":"FF0000","color2":"000000","speed":"normal","brightness":3}},"battery_limit":null,"battery_one_time_charge":false,"battery":{"charge_percent":null,"status":null,"power_microwatts":null,"health_percent":null},"panel_overdrive":null,"fan_curves":null,"fan_control_mode":"firmware","tdp":null,"undervolt":null,"cpu_temp_limit":null,"fan_hysteresis":{"up":3,"down":3},"fan_temperature_average_seconds":6,"direct_fan_duties":[0,0],"disable_high_power_fan_protection":false,"capabilities":{"ppd_profiles":[],"direct_fans":false,"undervolt":false},"telemetry":{"temperature_c":null,"fan_rpms":[0,0]},"health":{"degraded":false,"warnings":[]}}"#
        );
    }

    #[test]
    fn v3_status_drops_persisted_duplicates_and_inferred_protection() {
        let state = DaemonState {
            temperature: Some(10),
            fan_rpms: [11, 12],
            warnings: vec!["flat warning".into()],
            degraded: true,
            high_power_fan_protection: true,
            panel_overdrive: Some(1),
            lighting: LightingState {
                enabled: true,
                mode: "static".into(),
                ..LightingState::default()
            },
            devices: Some(std::collections::HashMap::from([(
                "keyboard".into(),
                LightingState {
                    enabled: false,
                    ..LightingState::default()
                },
            )])),
            telemetry: Telemetry {
                temperature_c: Some(42),
                fan_rpms: [101, 102],
            },
            health: Health {
                degraded: false,
                warnings: vec!["canonical warning".into()],
            },
            ..DaemonState::default()
        };

        let json = serde_json::to_value(wire_status(&state)).unwrap();
        assert_eq!(json["telemetry"]["temperature_c"], 42);
        assert_eq!(json["telemetry"]["fan_rpms"], serde_json::json!([101, 102]));
        assert_eq!(
            json["health"]["warnings"],
            serde_json::json!(["canonical warning"])
        );
        assert_eq!(json["devices"]["keyboard"]["enabled"], false);
        assert_eq!(json["devices"]["lightbar"]["enabled"], true);
        assert_eq!(json["panel_overdrive"], true);
        for field in [
            "temperature",
            "fan_rpms",
            "warnings",
            "degraded",
            "lighting",
            "high_power_fan_protection",
        ] {
            assert!(json.get(field).is_none(), "duplicate field {field} leaked");
        }
    }

    #[test]
    fn subscription_events_are_known_and_bounded() {
        assert!(validate_subscription_events(&[]).is_err());
        assert_eq!(
            validate_subscription_events(&[EventTopic::StateChanged]).unwrap(),
            vec![EventTopic::StateChanged]
        );
        assert!(
            validate_subscription_events(
                &(0..=MAX_SUBSCRIPTION_EVENTS)
                    .map(|_| EventTopic::StateChanged)
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
    }

    #[test]
    fn malformed_request_returns_protocol_error() {
        let response = failure(None, ErrorCode::Protocol, "bad request");
        let json = serde_json::to_value(response).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["error"]["code"], "protocol");
    }

    #[test]
    fn malformed_unknown_and_v2_frames_are_rejected_without_dispatch() {
        for line in [
            "not-json",
            r#"{"version":3,"client_id":"00000000000000000000000000000001","request_id":1,"cmd":"get-state","future":true}"#,
            r#"{"version":3,"client_id":"00000000000000000000000000000001","request_id":1,"cmd":"future-command"}"#,
            r#"{"version":2,"client_id":"00000000000000000000000000000001","request_id":1,"cmd":"get-state"}"#,
        ] {
            let Dispatch::Reply(reply) = parse_line(line) else {
                panic!("malformed frame was dispatched");
            };
            assert!(!reply.response.ok);
            assert_eq!(reply.response.error.unwrap().code, ErrorCode::Protocol);
        }
    }

    #[test]
    fn request_parser_accepts_whitespace_without_text_classification() {
        let line = r#"  { "version": 3, "client_id": "00000000000000000000000000000001", "request_id": 1, "cmd": "get-state" }  "#;
        let request = parse_request(line).unwrap();
        assert!(matches!(request.command, Command::GetState));
    }

    #[test]
    fn nested_strings_do_not_change_typed_effect_classification() {
        let request = WireRequest {
            version: PROTOCOL_VERSION,
            client_id: z13helper_core::protocol::ClientId::new(1).unwrap(),
            request_id: RequestId::new(6).unwrap(),
            command: Command::SetLighting {
                device: "keyboard \"cmd\":\"get-state\"".into(),
                state: LightingState::default(),
            },
        };
        let line = format!("  {}  ", serde_json::to_string(&request).unwrap());
        let parsed = parse_request(&line).unwrap();
        assert_eq!(command_effects(&parsed.command), vec![Effect::StateChanged]);
    }

    #[test]
    fn every_v3_command_has_an_exhaustive_effect_classification() {
        let apply = ApplyRequest::from_profile(&Profile::builtin("balanced", "Balanced"), false);
        let cases = [
            (Command::GetState, vec![]),
            (Command::Probe, vec![]),
            (
                Command::GetFactoryFanCurves {
                    ppd_profiles: vec![],
                },
                vec![],
            ),
            (
                Command::Apply { request: apply },
                vec![Effect::StateChanged],
            ),
            (
                Command::ApplyUndervoltOnce { offset: -20 },
                vec![Effect::StateChanged],
            ),
            (
                Command::SetBatteryLimit { limit: 80 },
                vec![Effect::StateChanged],
            ),
            (
                Command::SetBatteryOneTimeCharge { enabled: true },
                vec![Effect::StateChanged],
            ),
            (
                Command::SetPanelOverdrive { enabled: true },
                vec![Effect::StateChanged],
            ),
            (
                Command::SetLighting {
                    device: "keyboard".into(),
                    state: LightingState::default(),
                },
                vec![Effect::StateChanged],
            ),
            (Command::ReleaseFans, vec![Effect::StateChanged]),
            (
                Command::SetControllerCapture { enabled: true },
                vec![Effect::ControllerCapture(true)],
            ),
            (Command::Subscribe { events: vec![] }, vec![]),
            (
                Command::GetOutcome {
                    target_client_id: ClientId::new(1).unwrap(),
                    target_request_id: RequestId::new(7).unwrap(),
                },
                vec![],
            ),
        ];
        for (command, expected) in cases {
            assert_eq!(command_effects(&command), expected);
        }
    }
}
