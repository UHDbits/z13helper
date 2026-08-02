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

use anyhow::{bail, Context, Result};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use z13helper_core::protocol::{DaemonEvent, DaemonEventKind, WireResponse, PROTOCOL_VERSION};
use z13helperd::backend::Backend;
use z13helperd::ec::{EcMailbox, LinuxPortIo};
use z13helperd::input::spawn_button_watcher;
use z13helperd::protocol::{handle_line, Dispatch};
use z13helperd::resume::{spawn_resume_watcher, SleepEvent};
use z13helperd::service::{Controller, HwmonSensors};

mod logging;

const EXPECTED_MODEL: &str = "GZ302EA";
const DMI_PRODUCT_NAME: &str = "/sys/class/dmi/id/product_name";
const SOCKET_PATH: &str = "/run/z13helper/z13helperd.sock";
const MAX_REQUEST_BYTES: u64 = 64 * 1024;

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
                broadcast(&subscribers, DaemonEventKind::StateChanged);
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
    }
    Ok(())
}

fn broadcast(subscribers: &Arc<Mutex<Vec<Subscriber>>>, kind: DaemonEventKind) {
    let response = WireResponse {
        ok: true,
        state: None,
        apply: None,
        probe: None,
        factory_fan_curves: None,
        event: Some(DaemonEvent {
            kind,
            generation: None,
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
    let mut controller = Controller::new(EcMailbox::new(io), HwmonSensors);
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
    for signal in [SIGINT, SIGTERM, SIGHUP] {
        signal_hook::flag::register(signal, Arc::clone(&terminate))?;
    }

    let (event_tx, event_rx) = mpsc::channel();
    spawn_button_watcher(event_tx, Arc::clone(&terminate));
    let (sleep_tx, sleep_rx) = mpsc::channel();
    spawn_resume_watcher(sleep_tx);
    let mut next_tick = Instant::now();
    let mut next_hotplug = Instant::now();
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
                    thread::spawn(move || {
                        if let Err(error) = handle_client(stream, backend, subscribers) {
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
        if Instant::now() >= next_tick {
            backend.lock().unwrap().tick();
            next_tick = Instant::now() + Duration::from_secs(1);
        }
        if Instant::now() >= next_hotplug {
            backend.lock().unwrap().restore_hotplugged_lighting();
            next_hotplug = Instant::now() + Duration::from_secs(2);
        }
        while let Ok(event) = event_rx.try_recv() {
            broadcast(&subscribers, event);
        }
        while let Ok(event) = sleep_rx.try_recv() {
            match event {
                SleepEvent::Sleeping => backend.lock().unwrap().shutdown(),
                SleepEvent::Resumed => backend.lock().unwrap().restore_volatile(),
            }
        }
        thread::sleep(Duration::from_millis(25));
    }

    backend.lock().unwrap().shutdown();
    drop(listener);
    let _ = fs::remove_file(SOCKET_PATH);
    Ok(())
}
