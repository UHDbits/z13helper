use std::error::Error;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::ops::RangeInclusive;
use std::sync::mpsc::Receiver;

use z13helper_client::{Client, ClientId, DaemonError};
use z13helper_core::profile::default_cpu_temp_limit;
use z13helper_core::{
    ApplyRequest, Command, DaemonEvent, EventTopic, FanControlMode, LightingState, MAX_FRAME_BYTES,
    Profile, RequestId, TdpState,
};

const LIGHTING_BRIGHTNESS_RANGE: RangeInclusive<i32> = 0..=3;

const USAGE: &str = "z13helperctl commands:\n\
     status\n  probe\n  watch [event]\n  apply <json-file|->\n\
     outcome <client-id> <request-id>\n\
     ppd <profile|off>\n\
     tdp <pl1> <pl2> <fppt> [apu-sppt platform-sppt]\n  undervolt <-40..0|off>\n\
     fans off | fans <firmware|direct> <curves-json-file>\n\
     lighting <keyboard|lightbar> <off|mode> [color] [brightness]\n\
     battery-limit <40..100>\n  battery-charge-once <on|off>\n  panel-overdrive <on|off>\n\
     release-fans";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("expected on or off, got {value:?}")),
    }
}

fn expect_arity(
    command: &str,
    args: &[String],
    expected: RangeInclusive<usize>,
) -> Result<(), String> {
    if expected.contains(&args.len()) {
        return Ok(());
    }
    let (minimum, maximum) = expected.into_inner();
    let wanted = if minimum == maximum {
        format!("{minimum} argument{}", if minimum == 1 { "" } else { "s" })
    } else {
        format!("between {minimum} and {maximum} arguments")
    };
    Err(format!("{command}: expected {wanted}, got {}", args.len()))
}

fn parse_request_id(value: &str) -> Result<RequestId, String> {
    let request_id = value
        .parse::<u64>()
        .map_err(|error| format!("request ID must be an integer: {error}"))?;
    RequestId::try_from(request_id).map_err(String::from)
}

fn parse_i32(value: &str, name: &str) -> Result<i32, String> {
    value
        .parse::<i32>()
        .map_err(|error| format!("{name} must be an integer: {error}"))
}

fn parse_i32_in_range(value: &str, name: &str, range: RangeInclusive<i32>) -> Result<i32, String> {
    let parsed = parse_i32(value, name)?;
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

/// Applies core's range rules to one requested change, so bad input is
/// rejected before the daemon is contacted.
fn check(change: impl FnOnce(&mut ApplyRequest)) -> Result<(), String> {
    let mut request = ApplyRequest::from_profile(&Profile::builtin("balanced", "Balanced"), false);
    change(&mut request);
    request.validate()
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
    expect_arity("lighting", args, 2..=4)?;
    let device = args[0].clone();
    if !matches!(device.as_str(), "keyboard" | "lightbar") {
        return Err("lighting device must be keyboard or lightbar".into());
    }

    let mode = &args[1];
    if mode == "off" {
        expect_arity("lighting", args, 2..=2)?;
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
        cpu_temp_limit: state.cpu_temp_limit.unwrap_or_else(default_cpu_temp_limit),
        fan_hysteresis: state.fan_hysteresis,
        fan_temperature_average_seconds: state.fan_temperature_average_seconds,
        disable_high_power_fan_protection: state.disable_high_power_fan_protection,
    })
}

fn read_bounded<R: Read>(reader: R) -> Result<String, String> {
    let mut text = String::new();
    let mut reader = reader.take((MAX_FRAME_BYTES + 1) as u64);
    reader
        .read_to_string(&mut text)
        .map_err(|error| error.to_string())?;
    if text.len() > MAX_FRAME_BYTES {
        return Err(format!(
            "JSON input exceeds the {MAX_FRAME_BYTES}-byte protocol frame limit"
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

fn print_line(text: &str) -> Result<(), String> {
    match writeln!(std::io::stdout().lock(), "{text}") {
        // The reader went away (`z13helperctl watch | head -1`): output is done.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        result => result.map_err(|error| error.to_string()),
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<(), String> {
    print_line(&serde_json::to_string_pretty(value).map_err(|error| error.to_string())?)
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

fn run(args: impl IntoIterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    let mut args = args
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|arg| format!("argument {arg:?} is not valid UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let command = if args.is_empty() {
        "status".into()
    } else {
        args.remove(0)
    };
    let client = Client::new();
    match command.as_str() {
        "status" => {
            expect_arity("status", &args, 0..=0)?;
            print_json(&client.get_state()?)?;
        }
        "probe" => {
            expect_arity("probe", &args, 0..=0)?;
            print_json(&client.probe()?)?;
        }
        "outcome" => {
            expect_arity("outcome", &args, 2..=2)?;
            let client_id = args[0].parse::<ClientId>()?;
            let request_id = parse_request_id(&args[1])?;
            print_json(&client.get_outcome_for(client_id, request_id)?)?;
        }
        "watch" => {
            expect_arity("watch", &args, 0..=1)?;
            let event = args.first().map(String::as_str).unwrap_or("state-changed");
            let event = EventTopic::parse(event)
                .ok_or_else(|| format!("unknown subscription event {event:?}"))?;
            let (events, _cancel) = client.subscribe(&[event])?;
            watch_events(events)?;
        }
        "apply" => {
            expect_arity("apply", &args, 1..=1)?;
            let request: ApplyRequest = serde_json::from_str(&read_json(&args[0])?)?;
            print_json(&client.apply(request)?)?;
        }
        "ppd" => {
            expect_arity("ppd", &args, 1..=1)?;
            let ppd_profile = (args[0] != "off").then_some(args[0].clone());
            let mut request = current_request(&client)?;
            request.ppd_profile = ppd_profile;
            print_json(&client.apply(request)?)?;
        }
        "tdp" => {
            expect_arity("tdp", &args, 3..=5)?;
            let pl1_spl = parse_i32(&args[0], "PL1")?;
            let pl2_sppt = parse_i32(&args[1], "PL2")?;
            let fppt = parse_i32(&args[2], "FPPT")?;
            let apu_sppt = args
                .get(3)
                .map(|value| parse_i32(value, "APU SPPT"))
                .transpose()?
                .unwrap_or(pl2_sppt);
            let platform_sppt = args
                .get(4)
                .map(|value| parse_i32(value, "platform SPPT"))
                .transpose()?
                .unwrap_or(pl2_sppt);
            let power_limits = TdpState {
                pl1_spl,
                pl2_sppt,
                fppt,
                apu_sppt,
                platform_sppt,
            };
            check(|request| request.power_limits = Some(power_limits))?;
            let mut request = current_request(&client)?;
            request.power_limits = Some(power_limits);
            print_json(&client.apply(request)?)?;
        }
        "undervolt" => {
            expect_arity("undervolt", &args, 1..=1)?;
            let undervolt = (args[0] != "off")
                .then(|| parse_i32(&args[0], "undervolt"))
                .transpose()?;
            check(|request| request.undervolt = undervolt)?;
            let mut request = current_request(&client)?;
            request.undervolt = undervolt;
            print_json(&client.apply(request)?)?;
        }
        "fans" => {
            expect_arity("fans", &args, 1..=2)?;
            let mode = args[0].as_str();
            match mode {
                "off" => expect_arity("fans", &args, 1..=1)?,
                "firmware" | "direct" => expect_arity("fans", &args, 2..=2)?,
                _ => return Err("fan mode must be firmware, direct, or off".into()),
            }
            let fan_curves = match args.get(1) {
                Some(path) => Some(serde_json::from_str(&read_json(path)?)?),
                None => None,
            };
            let mut request = current_request(&client)?;
            request.fan_mode = if mode == "direct" {
                FanControlMode::Direct
            } else {
                FanControlMode::Firmware
            };
            // `off` takes no curves file, so this clears them.
            request.fan_curves = fan_curves;
            print_json(&client.apply(request)?)?;
        }
        "lighting" => {
            let (device, state) = parse_lighting(&args)?;
            client.send(Command::SetLighting { device, state })?;
            print_ok()?;
        }
        "battery-limit" => {
            expect_arity("battery-limit", &args, 1..=1)?;
            let limit = parse_i32_in_range(&args[0], "battery limit", 40..=100)?;
            client.send(Command::SetBatteryLimit { limit })?;
            print_ok()?;
        }
        "battery-charge-once" => {
            expect_arity("battery-charge-once", &args, 1..=1)?;
            let enabled = parse_bool(&args[0])?;
            client.send(Command::SetBatteryOneTimeCharge { enabled })?;
            print_ok()?;
        }
        "panel-overdrive" => {
            expect_arity("panel-overdrive", &args, 1..=1)?;
            let enabled = parse_bool(&args[0])?;
            client.send(Command::SetPanelOverdrive { enabled })?;
            print_ok()?;
        }
        "release-fans" => {
            expect_arity("release-fans", &args, 0..=0)?;
            client.send(Command::ReleaseFans)?;
            print_ok()?;
        }
        "help" | "--help" | "-h" => {
            expect_arity(&command, &args, 0..=0)?;
            print_line(USAGE)?;
        }
        _ => usage(),
    }
    Ok(())
}

fn main() {
    if let Err(error) = run(std::env::args_os().skip(1)) {
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
        let input = vec![b'x'; MAX_FRAME_BYTES + 1];
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
        assert!(expect_arity("status", &args, 0..=0).is_err());
        assert!(expect_arity("watch", &args, 0..=1).is_ok());
        assert!(expect_arity("watch", &["a".into(), "b".into()], 0..=1).is_err());
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
    fn battery_limit_range_is_checked_before_use() {
        assert!(parse_i32_in_range("39", "battery limit", 40..=100).is_err());
        assert!(parse_i32_in_range("100", "battery limit", 40..=100).is_ok());
    }

    #[test]
    fn power_and_undervolt_input_is_rejected_by_core_before_contacting_the_daemon() {
        // Only invalid input: it must fail on core's rules, not on a daemon
        // that is (or is not) running.
        for (args, message) in [
            (
                &["tdp", "94", "94", "94"][..],
                "PL1 must be between 5 and 93",
            ),
            (&["tdp", "30", "20", "40"], "PL1 <= PL2 <= FPPT"),
            (&["tdp", "20", "30", "25"], "PL1 <= PL2 <= FPPT"),
            (&["undervolt", "-41"], "between -40 and 0"),
        ] {
            let error = run(args.iter().map(OsString::from)).unwrap_err();
            assert!(error.to_string().contains(message), "{args:?}: {error}");
        }
        assert!(check(|request| request.undervolt = Some(-40)).is_ok());
    }

    #[test]
    fn non_utf8_argument_is_an_error() {
        use std::os::unix::ffi::OsStringExt;
        let error = run([std::ffi::OsString::from_vec(vec![0xff])]).unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
    }

    #[test]
    fn explicit_help_succeeds() {
        run(["help".into()]).unwrap();
    }

    #[test]
    fn closed_stdout_pipe_ends_output_cleanly() {
        // Re-runs this test in a child whose stdout reader goes away, like
        // `z13helperctl watch | head -1`.
        const CHILD: &str = "Z13HELPERCTL_TEST_CLOSED_STDOUT";
        if std::env::var_os(CHILD).is_some() {
            loop {
                print_ok().unwrap();
            }
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::closed_stdout_pipe_ends_output_cleanly",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        // Read until JSON flows, then drop the reader.
        std::io::BufRead::lines(stdout)
            .find(|line| line.as_ref().is_ok_and(|line| line.contains("\"ok\"")))
            .unwrap()
            .unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn bool_parser_accepts_only_documented_values() {
        assert_eq!(parse_bool("on"), Ok(true));
        assert!(parse_bool("true").is_err());
    }
}
