use std::io::Read;
use std::ops::RangeInclusive;
use std::sync::mpsc::Receiver;

use z13helper_client::{Client, ClientId, DaemonError};
use z13helper_core::{ApplyRequest, DaemonEvent, EventTopic, FanControlMode, LightingState};

const LIGHTING_BRIGHTNESS_RANGE: RangeInclusive<i32> = 0..=3;

fn usage() -> ! {
    eprintln!(
        "z13helperctl commands:\n\
         status\n  probe\n  watch [event]\n  apply <json-file|->\n\
         outcome <client-id> <request-id>\n\
         ppd <profile|off>\n\
         tdp <pl1> <pl2> <fppt> [apu-sppt platform-sppt]\n  undervolt <-40..0|off>\n\
         fans off | fans <firmware|direct> <curves-json-file>\n\
         lighting <keyboard|lightbar> <off|mode> [color] [brightness]\n\
         battery-limit <40..100>\n  battery-charge-once <on|off>\n  panel-overdrive <on|off>\n\
         release-fans"
    );
    std::process::exit(2)
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("expected on or off, got {value:?}")),
    }
}

fn expect_arity(command: &str, args: &[String], expected: usize) -> Result<(), String> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(format!(
            "{command}: expected {expected} argument{}, got {}",
            if expected == 1 { "" } else { "s" },
            args.len()
        ))
    }
}

fn expect_arity_between(
    command: &str,
    args: &[String],
    minimum: usize,
    maximum: usize,
) -> Result<(), String> {
    if (minimum..=maximum).contains(&args.len()) {
        Ok(())
    } else {
        Err(format!(
            "{command}: expected between {minimum} and {maximum} arguments, got {}",
            args.len()
        ))
    }
}

fn parse_request_id(value: &str) -> Result<u64, String> {
    let request_id = value
        .parse::<u64>()
        .map_err(|error| format!("request ID must be an integer: {error}"))?;
    if request_id == 0 {
        Err("request ID must be non-zero".into())
    } else {
        Ok(request_id)
    }
}

fn parse_i32_in_range(value: &str, name: &str, range: RangeInclusive<i32>) -> Result<i32, String> {
    let parsed = value
        .parse::<i32>()
        .map_err(|error| format!("{name} must be an integer: {error}"))?;
    if range.contains(&parsed) {
        Ok(parsed)
    } else {
        Err(format!(
            "{name} must be between {} and {}",
            range.start(),
            range.end()
        ))
    }
}

fn validate_lighting_color(value: &str) -> Result<(), String> {
    let color = value.strip_prefix('#').unwrap_or(value);
    if color.len() == 6 && color.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(format!(
            "lighting color must contain six hexadecimal digits, got {value:?}"
        ))
    }
}

fn validate_lighting_mode(value: &str) -> Result<(), String> {
    match value {
        "static" | "breathe" | "cycle" | "rainbow" | "strobe" => Ok(()),
        _ => Err(format!("unsupported lighting mode {value:?}")),
    }
}

fn parse_lighting(args: &[String]) -> Result<(String, LightingState), String> {
    expect_arity_between("lighting", args, 2, 4)?;
    let device = args[0].clone();
    if !matches!(device.as_str(), "keyboard" | "lightbar") {
        return Err("lighting device must be keyboard or lightbar".into());
    }

    let mode = &args[1];
    if mode == "off" {
        expect_arity("lighting", args, 2)?;
        return Ok((device, LightingState::default()));
    }

    let color = args.get(2).map(String::as_str).unwrap_or("FFFFFF");
    validate_lighting_mode(mode)?;
    validate_lighting_color(color)?;
    let brightness = args
        .get(3)
        .map(|value| parse_i32_in_range(value, "lighting brightness", LIGHTING_BRIGHTNESS_RANGE))
        .transpose()?
        .unwrap_or(3);
    Ok((
        device,
        LightingState {
            enabled: true,
            mode: mode.clone(),
            color: color.into(),
            color2: "000000".into(),
            speed: "normal".into(),
            brightness,
        },
    ))
}

fn current_request(client: &Client) -> Result<ApplyRequest, DaemonError> {
    let state = client.get_state()?;
    Ok(ApplyRequest {
        ppd_profile: state.ppd_profile,
        power_limits: state.overrides.power.then_some(state.tdp).flatten(),
        fan_mode: state.fan_control_mode,
        fan_curves: state.overrides.fans.then_some(state.fan_curves).flatten(),
        undervolt: state
            .overrides
            .undervolt
            .then_some(state.undervolt)
            .flatten()
            .map(|value| value.cpu_co),
        cpu_temp_limit: state.cpu_temp_limit.unwrap_or(95),
        fan_hysteresis: state.fan_hysteresis,
        fan_temperature_average_seconds: state.fan_temperature_average_seconds,
        disable_high_power_fan_protection: state.disable_high_power_fan_protection,
    })
}

fn read_bounded<R: Read>(reader: R) -> Result<String, String> {
    let mut text = String::new();
    let mut reader = reader.take((Client::MAX_FRAME_BYTES + 1) as u64);
    reader
        .read_to_string(&mut text)
        .map_err(|error| error.to_string())?;
    if text.len() > Client::MAX_FRAME_BYTES {
        return Err(format!(
            "JSON input exceeds the {}-byte protocol frame limit",
            Client::MAX_FRAME_BYTES
        ));
    }
    Ok(text)
}

fn read_json(path: &str) -> Result<String, String> {
    if path == "-" {
        read_bounded(std::io::stdin())
    } else {
        let file = std::fs::File::open(path).map_err(|error| format!("read {path}: {error}"))?;
        read_bounded(file).map_err(|error| format!("read {path}: {error}"))
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn print_ok() -> Result<(), String> {
    print_json(&serde_json::json!({ "ok": true }))
}

fn watch_events(events: Receiver<DaemonEvent>) -> Result<(), String> {
    loop {
        match events.recv() {
            Ok(event) => print_json(&event)?,
            Err(_) => return Err("watch subscription disconnected".into()),
        }
    }
}

fn run() -> Result<(), String> {
    let mut raw_args = std::env::args().skip(1).collect::<Vec<_>>();
    let command = raw_args.first().cloned().unwrap_or_else(|| "status".into());
    if !raw_args.is_empty() {
        raw_args.remove(0);
    }
    let args = raw_args;
    let client = Client::new();
    match command.as_str() {
        "status" => {
            expect_arity("status", &args, 0)?;
            print_json(&client.get_state().map_err(|error| error.to_string())?)?;
        }
        "probe" => {
            expect_arity("probe", &args, 0)?;
            print_json(&client.probe().map_err(|error| error.to_string())?)?;
        }
        "outcome" => {
            expect_arity("outcome", &args, 2)?;
            let client_id = args[0]
                .parse::<ClientId>()
                .map_err(|error| error.to_string())?;
            let request_id = parse_request_id(&args[1])?;
            print_json(
                &client
                    .get_outcome_for(client_id, request_id)
                    .map_err(|error| error.to_string())?,
            )?;
        }
        "watch" => {
            expect_arity_between("watch", &args, 0, 1)?;
            let event = args.first().map(String::as_str).unwrap_or("state-changed");
            let event = EventTopic::parse(event)
                .ok_or_else(|| format!("unknown subscription event {event:?}"))?;
            let (events, _cancel) = client
                .subscribe(&[event])
                .map_err(|error| error.to_string())?;
            watch_events(events)?;
        }
        "apply" => {
            expect_arity("apply", &args, 1)?;
            let text = read_json(&args[0])?;
            let request: ApplyRequest =
                serde_json::from_str(&text).map_err(|error| error.to_string())?;
            print_json(&client.apply(request).map_err(|error| error.to_string())?)?;
        }
        "ppd" => {
            expect_arity("ppd", &args, 1)?;
            let ppd_profile = (args[0] != "off").then_some(args[0].clone());
            let mut request = current_request(&client).map_err(|e| e.to_string())?;
            request.ppd_profile = ppd_profile;
            print_json(&client.apply(request).map_err(|error| error.to_string())?)?;
        }
        "tdp" => {
            expect_arity_between("tdp", &args, 3, 5)?;
            let pl1_spl = parse_i32_in_range(&args[0], "PL1", 5..=93)?;
            let pl2_sppt = parse_i32_in_range(&args[1], "PL2", 5..=93)?;
            let fppt = parse_i32_in_range(&args[2], "FPPT", 5..=120)?;
            let apu_sppt = args
                .get(3)
                .map(|value| parse_i32_in_range(value, "APU SPPT", 5..=93))
                .transpose()?
                .unwrap_or(pl2_sppt);
            let platform_sppt = args
                .get(4)
                .map(|value| parse_i32_in_range(value, "platform SPPT", 5..=93))
                .transpose()?
                .unwrap_or(pl2_sppt);
            if pl2_sppt < pl1_spl || fppt < pl2_sppt {
                return Err("power limits must satisfy PL1 <= PL2 <= FPPT".into());
            }
            let mut request = current_request(&client).map_err(|e| e.to_string())?;
            request.power_limits = Some(z13helper_core::TdpState {
                pl1_spl,
                pl2_sppt,
                fppt,
                apu_sppt,
                platform_sppt,
            });
            print_json(&client.apply(request).map_err(|error| error.to_string())?)?;
        }
        "undervolt" => {
            expect_arity("undervolt", &args, 1)?;
            let undervolt = if args[0] == "off" {
                None
            } else {
                Some(parse_i32_in_range(&args[0], "undervolt", -40..=0)?)
            };
            let mut request = current_request(&client).map_err(|e| e.to_string())?;
            request.undervolt = undervolt;
            print_json(&client.apply(request).map_err(|error| error.to_string())?)?;
        }
        "fans" => {
            expect_arity_between("fans", &args, 1, 2)?;
            let mode = args[0].as_str();
            if mode == "off" {
                expect_arity("fans", &args, 1)?;
            } else if !matches!(mode, "firmware" | "direct") {
                return Err("fan mode must be firmware, direct, or off".into());
            } else {
                expect_arity("fans", &args, 2)?;
            }
            let fan_curves = args
                .get(1)
                .map(|path| {
                    serde_json::from_str(&read_json(path)?).map_err(|error| error.to_string())
                })
                .transpose()?;
            let mut request = current_request(&client).map_err(|e| e.to_string())?;
            match mode {
                "off" => {
                    request.fan_mode = FanControlMode::Firmware;
                    request.fan_curves = None;
                }
                "firmware" | "direct" => {
                    request.fan_mode = if mode == "direct" {
                        FanControlMode::Direct
                    } else {
                        FanControlMode::Firmware
                    };
                    request.fan_curves = fan_curves;
                }
                _ => unreachable!("fan mode checked above"),
            }
            print_json(&client.apply(request).map_err(|error| error.to_string())?)?;
        }
        "lighting" => {
            let (device, state) = parse_lighting(&args)?;
            client
                .apply_lighting(&device, state)
                .map_err(|e| e.to_string())?;
            print_ok()?;
        }
        "battery-limit" => {
            expect_arity("battery-limit", &args, 1)?;
            client
                .battery_limit_set(parse_i32_in_range(&args[0], "battery limit", 40..=100)?)
                .map_err(|e| e.to_string())?;
            print_ok()?;
        }
        "battery-charge-once" => {
            expect_arity("battery-charge-once", &args, 1)?;
            client
                .battery_one_time_charge_set(parse_bool(&args[0])?)
                .map_err(|e| e.to_string())?;
            print_ok()?;
        }
        "panel-overdrive" => {
            expect_arity("panel-overdrive", &args, 1)?;
            client
                .panel_overdrive_set(parse_bool(&args[0])?)
                .map_err(|e| e.to_string())?;
            print_ok()?;
        }
        "release-fans" => {
            expect_arity("release-fans", &args, 0)?;
            client.release_fans().map_err(|e| e.to_string())?;
            print_ok()?;
        }
        "help" | "--help" | "-h" => {
            expect_arity(&command, &args, 0)?;
            usage();
        }
        _ => usage(),
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("z13helperctl: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn stdin_is_bounded_by_the_client_frame_limit() {
        let input = vec![b'x'; Client::MAX_FRAME_BYTES + 1];
        let error = read_bounded(Cursor::new(input)).unwrap_err();
        assert!(error.contains("protocol frame limit"));
    }

    #[test]
    fn lighting_brightness_must_be_in_hardware_range() {
        let args = vec![
            "keyboard".into(),
            "static".into(),
            "FFFFFF".into(),
            "4".into(),
        ];
        let error = parse_lighting(&args).unwrap_err();
        assert!(error.contains("between 0 and 3"));
        let args = vec!["keyboard".into(), "off".into(), "FFFFFF".into()];
        assert!(parse_lighting(&args).is_err());
    }

    #[test]
    fn command_arity_rejects_trailing_arguments() {
        let args = vec!["unexpected".into()];
        assert!(expect_arity("status", &args, 0).is_err());
        assert!(expect_arity_between("watch", &args, 0, 1).is_ok());
        assert!(expect_arity_between("watch", &["a".into(), "b".into()], 0, 1).is_err());
    }

    #[test]
    fn watch_disconnect_is_an_error() {
        let (sender, receiver) = std::sync::mpsc::channel();
        drop(sender);
        assert_eq!(
            watch_events(receiver).unwrap_err(),
            "watch subscription disconnected"
        );
    }

    #[test]
    fn documented_numeric_ranges_are_checked_before_use() {
        assert!(parse_i32_in_range("39", "battery limit", 40..=100).is_err());
        assert!(parse_i32_in_range("-41", "undervolt", -40..=0).is_err());
        assert!(parse_i32_in_range("94", "PL1", 5..=93).is_err());
    }

    #[test]
    fn bool_parser_accepts_only_documented_values() {
        assert_eq!(parse_bool("on"), Ok(true));
        assert!(parse_bool("true").is_err());
    }
}
