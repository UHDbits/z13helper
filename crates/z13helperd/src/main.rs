use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use z13helper_core::protocol::{
    ControllerAction, DaemonEvent, DaemonEventKind, PROTOCOL_VERSION, WireResponse,
};
use z13helperd::backend::Backend;
use z13helperd::ec::{EcMailbox, LinuxPortIo};
use z13helperd::input::{spawn_button_watcher, spawn_controller_watcher};
use z13helperd::protocol::{Dispatch, handle_line};
use z13helperd::resume::{SleepEvent, spawn_resume_watcher};
use z13helperd::service::{Controller, DIRECT_TICK_INTERVAL};
use z13helperd::steam::SteamBlocker;

mod logging;

const EXPECTED_MODEL: &str = "GZ302EA";
const DMI_PRODUCT_NAME: &str = "/sys/class/dmi/id/product_name";
const SOCKET_PATH: &str = "/run/z13helper/z13helperd.sock";
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
const CONTROLLER_CAPTURE_LEASE: Duration = Duration::from_secs(3);

struct Subscriber {
    events: Vec<String>,
    stream: UnixStream,
}

fn verify_model() -> Result<()> {
    let model = fs::read_to_string(DMI_PRODUCT_NAME)
        .with_context(|| format!("read DMI product name from {DMI_PRODUCT_NAME}"))?;
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
    let parent = path.parent().context("daemon socket has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o660))
        .with_context(|| format!("chmod {}", path.display()))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn write_response(stream: &mut UnixStream, response: &WireResponse) -> Result<()> {
    let body = serde_json::to_vec(response)?;
    stream.write_all(&body)?;
    stream.write_all(b"\n")?;
    Ok(())
}

fn handle_client(
    mut stream: UnixStream,
    backend: Arc<Mutex<Backend>>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    controller_capture_deadline: Arc<Mutex<Option<Instant>>>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut line = String::new();
    BufReader::new(&stream)
        .take(MAX_REQUEST_BYTES)
        .read_line(&mut line)?;
    if line.is_empty() {
        return Ok(());
    }
    if !line.ends_with('\n') {
        bail!("request exceeded 64 KiB or was not newline terminated");
    }
    let dispatch = handle_line(&mut backend.lock().unwrap(), line.trim_end());
    match dispatch {
        Dispatch::Reply(response) => {
            let mutating = response.ok
                && !line.contains("\"cmd\":\"get-state\"")
                && !line.contains("\"cmd\":\"probe\"");
            write_response(&mut stream, &response)?;
            if mutating {
                broadcast(&subscribers, DaemonEventKind::StateChanged, None, None);
            }
        }
        Dispatch::Subscribe(events) => {
            write_response(&mut stream, &WireResponse::success())?;
            stream.set_read_timeout(None)?;
            subscribers
                .lock()
                .unwrap()
                .push(Subscriber { events, stream });
        }
        Dispatch::ControllerCapture(enabled) => {
            *controller_capture_deadline.lock().unwrap() =
                enabled.then(|| Instant::now() + CONTROLLER_CAPTURE_LEASE);
            write_response(&mut stream, &WireResponse::success())?;
        }
    }
    Ok(())
}

fn broadcast(
    subscribers: &Arc<Mutex<Vec<Subscriber>>>,
    kind: DaemonEventKind,
    action: Option<ControllerAction>,
    on_battery: Option<bool>,
) {
    let response = WireResponse {
        ok: true,
        state: None,
        apply: None,
        probe: None,
        factory_fan_curves: None,
        event: Some(DaemonEvent {
            kind,
            action,
            generation: None,
            on_battery,
        }),
        error: None,
    };
    let body = serde_json::to_vec(&response).unwrap_or_default();
    subscribers.lock().unwrap().retain_mut(|subscriber| {
        if !subscriber
            .events
            .iter()
            .any(|wanted| wanted == kind.as_str())
        {
            return true;
        }
        subscriber.stream.write_all(&body).is_ok() && subscriber.stream.write_all(b"\n").is_ok()
    });
}

fn release_ec_only() -> Result<()> {
    verify_model()?;
    let io = LinuxPortIo::acquire().context("acquire EC mailbox ports")?;
    let mut controller = Controller::new(EcMailbox::new(io));
    controller
        .release()
        .map_err(anyhow::Error::msg)
        .context("release EC automatic mode")
}

fn main() -> Result<()> {
    logging::init();
    if std::env::args().any(|argument| argument == "--release-ec") {
        return release_ec_only();
    }
    verify_model()?;
    let backend = Arc::new(Mutex::new(Backend::start().map_err(anyhow::Error::msg)?));
    let listener = bind_socket(Path::new(SOCKET_PATH))?;
    let subscribers = Arc::new(Mutex::new(Vec::new()));
    let terminate = Arc::new(AtomicBool::new(false));
    let controller_capture_deadline = Arc::new(Mutex::new(None::<Instant>));
    for signal in [SIGINT, SIGTERM, SIGHUP] {
        signal_hook::flag::register(signal, Arc::clone(&terminate))?;
    }

    let (event_tx, event_rx) = mpsc::channel();
    spawn_button_watcher(event_tx, Arc::clone(&terminate));
    let (controller_tx, controller_rx) = mpsc::channel();
    let controller_capture = spawn_controller_watcher(controller_tx, Arc::clone(&terminate))?;
    let (sleep_tx, sleep_rx) = mpsc::channel();
    spawn_resume_watcher(sleep_tx);
    let mut next_direct_tick = Instant::now();
    let mut next_observe = Instant::now();
    let mut next_hotplug = Instant::now();
    let mut next_steam_retry = Instant::now();
    let mut controller_capture_enabled = false;
    let mut steam_blocker = SteamBlocker::new();
    tracing::info!(
        socket = SOCKET_PATH,
        protocol = PROTOCOL_VERSION,
        "z13helperd ready"
    );

    while !terminate.load(Ordering::Relaxed) {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let backend = Arc::clone(&backend);
                    let subscribers = Arc::clone(&subscribers);
                    let controller_capture_deadline = Arc::clone(&controller_capture_deadline);
                    thread::spawn(move || {
                        if let Err(error) =
                            handle_client(stream, backend, subscribers, controller_capture_deadline)
                        {
                            tracing::warn!(%error, "client request failed");
                        }
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    tracing::error!(%error, "socket accept failed");
                    break;
                }
            }
        }
        if Instant::now() >= next_direct_tick {
            backend.lock().unwrap().tick();
            next_direct_tick = Instant::now() + DIRECT_TICK_INTERVAL;
        }
        if Instant::now() >= next_observe {
            backend.lock().unwrap().observe();
            next_observe = Instant::now() + Duration::from_secs(1);
        }
        if Instant::now() >= next_hotplug {
            backend.lock().unwrap().restore_hotplugged_lighting();
            next_hotplug = Instant::now() + Duration::from_secs(2);
        }
        while let Ok(event) = event_rx.try_recv() {
            broadcast(&subscribers, event, None, None);
        }
        while let Ok(action) = controller_rx.try_recv() {
            broadcast(
                &subscribers,
                DaemonEventKind::ControllerAction,
                Some(action),
                None,
            );
        }
        while let Ok(event) = sleep_rx.try_recv() {
            match event {
                SleepEvent::Sleeping => backend.lock().unwrap().shutdown(),
                SleepEvent::Resumed => {
                    let power_source_changed = backend.lock().unwrap().restore_volatile();
                    if let Some(on_battery) = power_source_changed {
                        broadcast(
                            &subscribers,
                            DaemonEventKind::PowerSourceChanged,
                            None,
                            Some(on_battery),
                        );
                    }
                }
            }
        }
        let capture_requested = controller_capture_deadline
            .lock()
            .unwrap()
            .is_some_and(|deadline| deadline > Instant::now());
        if capture_requested != controller_capture_enabled {
            if capture_requested {
                steam_blocker.block();
                next_steam_retry = Instant::now() + Duration::from_secs(2);
                match controller_capture.set_enabled(true) {
                    Ok(()) => controller_capture_enabled = true,
                    Err(error) => {
                        steam_blocker.unblock();
                        tracing::warn!(%error, "could not enable controller capture")
                    }
                }
            } else {
                match controller_capture.set_enabled(false) {
                    Ok(()) => {
                        controller_capture_enabled = false;
                        steam_blocker.unblock();
                    }
                    Err(error) => {
                        tracing::warn!(%error, "could not disable controller capture")
                    }
                }
            }
        }
        // Refresh the Steam PID set while capture stays leased so late-started
        // Steam helpers and newly spawned children are blocked too.
        if capture_requested && controller_capture_enabled && Instant::now() >= next_steam_retry {
            steam_blocker.block();
            next_steam_retry = Instant::now() + Duration::from_secs(2);
        }
        thread::sleep(Duration::from_millis(25));
    }

    let _ = controller_capture.set_enabled(false);
    steam_blocker.unblock();
    backend.lock().unwrap().shutdown();
    drop(listener);
    let _ = fs::remove_file(SOCKET_PATH);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn system_unit_grants_only_required_bpf_capabilities() {
        let unit = include_str!("../../../contrib/systemd/z13helperd.service");
        assert!(unit.contains("CAP_SYS_RAWIO CAP_BPF CAP_PERFMON"));
        assert!(unit.contains("DeviceAllow=char-hidraw rw"));
        assert!(unit.contains("LimitMEMLOCK=infinity"));
        assert!(unit.contains("MemoryDenyWriteExecute=no"));
        assert!(
            !unit
                .lines()
                .any(|line| line.starts_with("ProtectProc=") || line.starts_with("ProcSubset="))
        );
    }
}
