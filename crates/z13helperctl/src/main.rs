use std::io::Read;

use z13helper_client::Client;
use z13helper_core::{ApplyRequest, DaemonError, FanControlMode, LightingState};

fn usage() -> ! {
    eprintln!(
        "z13helperctl commands:\n\
         status\n  probe\n  watch [event]\n  apply <json-file|->\n\
         ppd <profile|off>\n\
         tdp <pl1> <pl2> <fppt> [apu-sppt platform-sppt]\n  undervolt <-40..0|off>\n\
         fans <firmware|direct|off> [curves-json-file]\n\
         lighting <keyboard|lightbar> <off|mode> [color] [brightness]\n\
         battery-limit <40..100>\n  battery-charge-once <on|off>\n  panel-overdrive <on|off>\n\
         release-fans"
    );
    std::process::exit(2)
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value {
        "on" | "true" | "1" => Ok(true),
        "off" | "false" | "0" => Ok(false),
        _ => Err(format!("expected on or off, got {value:?}")),
    }
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
        disable_high_power_fan_protection: state.disable_high_power_fan_protection,
    })
}

fn read_json(path: &str) -> Result<String, String> {
    if path == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|error| error.to_string())?;
        Ok(text)
    } else {
        std::fs::read_to_string(path).map_err(|error| format!("read {path}: {error}"))
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

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "status".into());
    let client = Client::new();
    match command.as_str() {
        "status" => print_json(&client.get_state().map_err(|error| error.to_string())?)?,
        "probe" => print_json(&client.probe().map_err(|error| error.to_string())?)?,
        "watch" => {
            let event = args.next().unwrap_or_else(|| "state-changed".into());
            let (events, _cancel) = client
                .subscribe(&[&event])
                .map_err(|error| error.to_string())?;
            while let Ok(event) = events.recv() {
                print_json(&event)?;
            }
        }
        "apply" => {
            let text = read_json(&args.next().unwrap_or_else(|| usage()))?;
            let request: ApplyRequest =
                serde_json::from_str(&text).map_err(|error| error.to_string())?;
            print_json(&client.apply(request).map_err(|error| error.to_string())?)?;
        }
        "ppd" => {
            let mut request = current_request(&client).map_err(|e| e.to_string())?;
            let value = args.next().unwrap_or_else(|| usage());
            request.ppd_profile = (value != "off").then_some(value);
            print_json(&client.apply(request).map_err(|error| error.to_string())?)?;
        }
        "tdp" => {
            let mut request = current_request(&client).map_err(|e| e.to_string())?;
            let values = (0..3)
                .map(|_| {
                    args.next()
                        .unwrap_or_else(|| usage())
                        .parse::<i32>()
                        .map_err(|error| error.to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            let apu_sppt = args
                .next()
                .map(|value| value.parse::<i32>().map_err(|error| error.to_string()))
                .transpose()?
                .unwrap_or(values[1]);
            let platform_sppt = args
                .next()
                .map(|value| value.parse::<i32>().map_err(|error| error.to_string()))
                .transpose()?
                .unwrap_or(values[1]);
            request.power_limits = Some(z13helper_core::TdpState {
                pl1_spl: values[0],
                pl2_sppt: values[1],
                fppt: values[2],
                apu_sppt,
                platform_sppt,
            });
            print_json(&client.apply(request).map_err(|error| error.to_string())?)?;
        }
        "undervolt" => {
            let mut request = current_request(&client).map_err(|e| e.to_string())?;
            let value = args.next().unwrap_or_else(|| usage());
            request.undervolt = if value == "off" {
                None
            } else {
                Some(value.parse::<i32>().map_err(|error| error.to_string())?)
            };
            print_json(&client.apply(request).map_err(|error| error.to_string())?)?;
        }
        "fans" => {
            let mut request = current_request(&client).map_err(|e| e.to_string())?;
            let mode = args.next().unwrap_or_else(|| usage());
            match mode.as_str() {
                "off" => request.fan_curves = None,
                "firmware" | "direct" => {
                    request.fan_mode = if mode == "direct" {
                        FanControlMode::Direct
                    } else {
                        FanControlMode::Firmware
                    };
                    if let Some(path) = args.next() {
                        request.fan_curves = Some(
                            serde_json::from_str(&read_json(&path)?)
                                .map_err(|error| error.to_string())?,
                        );
                    }
                }
                _ => return Err("fan mode must be firmware, direct, or off".into()),
            }
            print_json(&client.apply(request).map_err(|error| error.to_string())?)?;
        }
        "lighting" => {
            let device = args.next().unwrap_or_else(|| usage());
            let mode = args.next().unwrap_or_else(|| usage());
            let state = if mode == "off" {
                LightingState::default()
            } else {
                LightingState {
                    enabled: true,
                    mode,
                    color: args.next().unwrap_or_else(|| "FFFFFF".into()),
                    color2: "000000".into(),
                    speed: "normal".into(),
                    brightness: args.next().and_then(|v| v.parse().ok()).unwrap_or(3),
                }
            };
            if state.enabled {
                client
                    .apply_lighting(
                        &state.mode,
                        &state.color,
                        &state.color2,
                        &state.speed,
                        state.brightness,
                        &device,
                    )
                    .map_err(|e| e.to_string())?;
            } else {
                client.lighting_off(&device).map_err(|e| e.to_string())?;
            }
            print_ok()?;
        }
        "battery-limit" => {
            client
                .battery_limit_set(
                    args.next()
                        .unwrap_or_else(|| usage())
                        .parse()
                        .map_err(|error: std::num::ParseIntError| error.to_string())?,
                )
                .map_err(|e| e.to_string())?;
            print_ok()?;
        }
        "battery-charge-once" => {
            client
                .battery_one_time_charge_set(parse_bool(&args.next().unwrap_or_else(|| usage()))?)
                .map_err(|e| e.to_string())?;
            print_ok()?;
        }
        "panel-overdrive" => {
            client
                .panel_overdrive_set(i32::from(parse_bool(
                    &args.next().unwrap_or_else(|| usage()),
                )?))
                .map_err(|e| e.to_string())?;
            print_ok()?;
        }
        "release-fans" => {
            client.release_fans().map_err(|e| e.to_string())?;
            print_ok()?;
        }
        "help" | "--help" | "-h" => usage(),
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
