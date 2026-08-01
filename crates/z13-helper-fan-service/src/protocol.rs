use serde::{Deserialize, Serialize};

use crate::curve::{Curve, Point};

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(alias = "command")]
    cmd: String,
    #[serde(default)]
    points: Option<Vec<LegacyPoint>>,
    #[serde(default)]
    curves: Option<[Curve; 2]>,
}

#[derive(Debug, Deserialize)]
struct LegacyPoint {
    temp: i32,
    pwm: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProbeReply {
    pub model: String,
    pub ec_version: u8,
    pub fan_count: u8,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StatusReply {
    pub enabled: bool,
    pub apu_temperature_c: i32,
    pub rpm: [u32; 2],
    pub pl1_w: Option<u32>,
    pub duty: [u8; 2],
    pub ec_errors: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ClientStatus {
    available: bool,
    active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_pwm: Option<i32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fan_rpm: Vec<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pl1_w: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fail_safe: Option<String>,
}

impl ClientStatus {
    fn available() -> Self {
        Self {
            available: true,
            active: false,
            target_pwm: None,
            fan_rpm: Vec::new(),
            temperature: None,
            pl1_w: None,
            fail_safe: None,
        }
    }
}

impl From<StatusReply> for ClientStatus {
    fn from(status: StatusReply) -> Self {
        Self {
            available: true,
            active: status.enabled,
            target_pwm: status
                .enabled
                .then(|| i32::from(status.duty[0].max(status.duty[1]))),
            fan_rpm: status.rpm.into_iter().map(|rpm| rpm as i32).collect(),
            temperature: Some(status.apu_temperature_c),
            pl1_w: status.pl1_w,
            fail_safe: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct WireResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<ClientStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe: Option<ProbeReply>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl WireResponse {
    fn success(status: Option<ClientStatus>, probe: Option<ProbeReply>) -> Self {
        Self {
            ok: true,
            status,
            probe,
            error: None,
        }
    }

    fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            status: None,
            probe: None,
            error: Some(error.into()),
        }
    }
}

pub trait FanBroker {
    fn probe(&mut self) -> Result<ProbeReply, String>;
    fn status(&mut self) -> Result<StatusReply, String>;
    fn enable(&mut self, curves: [Curve; 2]) -> Result<(), String>;
    fn release(&mut self) -> Result<(), String>;
}

fn requested_curves(request: Request) -> Result<[Curve; 2], String> {
    if let Some(curves) = request.curves {
        return Ok(curves);
    }
    let points = request
        .points
        .ok_or_else(|| "enable requires points or curves".to_owned())?
        .into_iter()
        .map(|point| {
            Ok(Point {
                temperature_c: point.temp,
                duty: u8::try_from(point.pwm)
                    .map_err(|_| format!("PWM {} is outside 0..=255", point.pwm))?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let curve = Curve::try_from(points).map_err(|error| error.to_string())?;
    Ok([curve.clone(), curve])
}

fn dispatch(broker: &mut impl FanBroker, request: Request) -> Result<WireResponse, String> {
    match request.cmd.as_str() {
        "probe" => broker
            .probe()
            .map(|probe| WireResponse::success(Some(ClientStatus::available()), Some(probe))),
        "status" => broker
            .status()
            .map(|status| WireResponse::success(Some(status.into()), None)),
        "enable" => {
            let curves = requested_curves(request)?;
            broker.enable(curves)?;
            broker
                .status()
                .map(|status| WireResponse::success(Some(status.into()), None))
        }
        "release" => broker.release().map(|()| WireResponse::success(None, None)),
        command => Err(format!("unknown command {command:?}")),
    }
}

pub fn handle_line(broker: &mut impl FanBroker, line: &str) -> String {
    let wire = match serde_json::from_str::<Request>(line) {
        Ok(request) => dispatch(broker, request).unwrap_or_else(WireResponse::failure),
        Err(error) => WireResponse::failure(format!("invalid request: {error}")),
    };
    serde_json::to_string(&wire).expect("wire response is serializable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockBroker {
        enabled: bool,
        released: bool,
    }

    impl FanBroker for MockBroker {
        fn probe(&mut self) -> Result<ProbeReply, String> {
            Ok(ProbeReply {
                model: "GZ302EA".to_owned(),
                ec_version: 7,
                fan_count: 2,
            })
        }

        fn status(&mut self) -> Result<StatusReply, String> {
            Ok(StatusReply {
                enabled: self.enabled,
                apu_temperature_c: 50,
                rpm: [3000, 3100],
                pl1_w: Some(80),
                duty: [100, 100],
                ec_errors: 0,
            })
        }

        fn enable(&mut self, curves: [Curve; 2]) -> Result<(), String> {
            self.enabled = curves[0].duty_at(50) == 100 && curves[1].duty_at(50) == 100;
            Ok(())
        }

        fn release(&mut self) -> Result<(), String> {
            self.released = true;
            Ok(())
        }
    }

    fn legacy_points_json() -> String {
        serde_json::to_string(
            &(0..8)
                .map(|index| {
                    serde_json::json!({
                        "temp": 30 + index * 10,
                        "pwm": 60 + index * 20,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn handles_probe_ndjson_request() {
        let value: serde_json::Value = serde_json::from_str(&handle_line(
            &mut MockBroker::default(),
            r#"{"cmd":"probe"}"#,
        ))
        .unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["status"]["available"], true);
        assert_eq!(value["probe"]["model"], "GZ302EA");
    }

    #[test]
    fn validates_and_enables_legacy_eight_point_curve_for_both_fans() {
        let points = legacy_points_json();
        let line = format!(r#"{{"cmd":"enable","points":{points}}}"#);
        let mut broker = MockBroker::default();
        let value: serde_json::Value =
            serde_json::from_str(&handle_line(&mut broker, &line)).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["status"]["active"], true);
        assert!(broker.enabled);
    }

    #[test]
    fn malformed_request_returns_one_error_object() {
        let value: serde_json::Value =
            serde_json::from_str(&handle_line(&mut MockBroker::default(), "{")).unwrap();
        assert_eq!(value["ok"], false);
        assert!(value["error"]
            .as_str()
            .unwrap()
            .starts_with("invalid request:"));
    }
}
