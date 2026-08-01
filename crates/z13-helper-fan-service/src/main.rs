use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use z13_helper_fan_service::ec::{EcMailbox, LinuxPortIo};
use z13_helper_fan_service::protocol::handle_line;
use z13_helper_fan_service::service::{Controller, HwmonSensors};

const EXPECTED_MODEL: &str = "GZ302EA";
const DMI_PRODUCT_NAME: &str = "/sys/class/dmi/id/product_name";
const SOCKET_PATH: &str = "/run/z13-helper/fan.sock";
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
const CONTROL_INTERVAL: Duration = Duration::from_secs(1);

fn verify_model() -> Result<()> {
    let model = fs::read_to_string(DMI_PRODUCT_NAME)
        .with_context(|| format!("read DMI product name from {DMI_PRODUCT_NAME}"))?;
    // Product names look like "ROG Flow Z13 GZ302EA_GZ302EA".
    if !model.to_ascii_uppercase().contains(EXPECTED_MODEL) {
        bail!(
            "unsupported DMI product {:?}; expected a {EXPECTED_MODEL} model",
            model.trim()
        );
    }
    Ok(())
}

fn bind_socket(path: &Path) -> Result<UnixListener> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() {
            bail!("refusing to replace non-socket {}", path.display());
        }
        fs::remove_file(path).with_context(|| format!("remove stale socket {}", path.display()))?;
    }
    let parent = path
        .parent()
        .context("fan service socket path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create runtime directory {}", parent.display()))?;
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o770))
        .with_context(|| format!("set permissions on {}", path.display()))?;
    listener
        .set_nonblocking(true)
        .context("make fan socket nonblocking")?;
    Ok(listener)
}

fn handle_client(
    mut stream: UnixStream,
    controller: &mut Controller<LinuxPortIo, HwmonSensors>,
) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .context("set client read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .context("set client write timeout")?;

    let mut line = String::new();
    BufReader::new(&stream)
        .take(MAX_REQUEST_BYTES)
        .read_line(&mut line)
        .context("read NDJSON request")?;
    if line.is_empty() {
        return Ok(());
    }
    if !line.ends_with('\n') {
        bail!("request exceeded limit or was not newline terminated");
    }
    let response = handle_line(controller, line.trim_end());
    stream
        .write_all(response.as_bytes())
        .and_then(|()| stream.write_all(b"\n"))
        .context("write NDJSON response")
}

fn main() -> Result<()> {
    verify_model()?;

    let terminate = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM, SIGHUP] {
        signal_hook::flag::register(signal, Arc::clone(&terminate))
            .with_context(|| format!("register signal {signal}"))?;
    }

    let port_io = LinuxPortIo::acquire()
        .context("acquire EC mailbox ports (the service requires only CAP_SYS_RAWIO)")?;
    let mut controller = Controller::new(EcMailbox::new(port_io), HwmonSensors);
    let probe = controller
        .startup_release_and_probe()
        .map_err(anyhow::Error::msg)
        .context("release EC to automatic mode and probe mailbox")?;
    eprintln!(
        "fan service ready: model={}, EC version={}, fans={}",
        probe.model, probe.ec_version, probe.fan_count
    );

    let socket_path = Path::new(SOCKET_PATH);
    let listener = bind_socket(socket_path)?;
    let mut next_tick = Instant::now() + CONTROL_INTERVAL;

    while !terminate.load(Ordering::Relaxed) {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Err(error) = handle_client(stream, &mut controller) {
                        eprintln!("client request failed: {error:#}");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    eprintln!("socket accept failed: {error}");
                    break;
                }
            }
        }

        if Instant::now() >= next_tick {
            if let Err(error) = controller.tick() {
                eprintln!("fan control tick failed: {error}");
            }
            next_tick = Instant::now() + CONTROL_INTERVAL;
        }
        thread::sleep(Duration::from_millis(25));
    }

    if let Err(error) = controller.shutdown_release() {
        eprintln!("failed to release EC on shutdown: {error}");
    }
    drop(listener);
    if let Err(error) = fs::remove_file(socket_path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            eprintln!("failed to remove {}: {error}", socket_path.display());
        }
    }
    Ok(())
}
