//! Connectivity smoke test against the live z13ctl daemon.
//!
//! Run with: `cargo run -p z13ctl-client --bin z13ctl-smoke`

use z13ctl_client::{Client, DaemonError};

fn main() {
    let client = Client::new();
    println!("socket: {}", Client::socket_path());

    match client.get_state() {
        Ok(state) => {
            println!("ok: connected");
            println!(
                "  profile={:?} undervolt_available={} temp={:?} fan_rpm={:?}",
                state.profile, state.undervolt_available, state.temperature, state.fan_rpm
            );
            match client.profile_get() {
                Ok(base) => println!("  platform_profile (stock base)={base}"),
                Err(e) => println!("  profile-get error: {e}"),
            }
            std::process::exit(0);
        }
        Err(DaemonError::NotRunning) => {
            eprintln!("FAIL: {}", DaemonError::NotRunning);
            eprintln!("hint: systemctl --user enable --now z13ctl.socket z13ctl.service");
            std::process::exit(1);
        }
        Err(DaemonError::PermissionDenied) => {
            eprintln!("FAIL: {}", DaemonError::PermissionDenied);
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("FAIL: {e}");
            std::process::exit(3);
        }
    }
}
