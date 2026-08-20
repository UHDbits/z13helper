use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use z13helper_core::error::ErrorCode;
use z13helper_core::protocol::{
    ControllerAction, DaemonEvent, DaemonEventKind, PROTOCOL_VERSION, WireResponse,
};
use z13helperd::backend::Backend;
use z13helperd::ec::{EcMailbox, LinuxPortIo};
use z13helperd::input::{spawn_button_watcher, spawn_controller_watcher};
use z13helperd::protocol::{Dispatch, failure, handle_line};
use z13helperd::resume::{SleepEvent, spawn_resume_watcher};
use z13helperd::service::{Controller, DIRECT_TICK_INTERVAL};
use z13helperd::steam::SteamBlocker;

mod logging;

const EXPECTED_MODEL: &str = "GZ302EA";
const DMI_PRODUCT_NAME: &str = "/sys/class/dmi/id/product_name";
const SOCKET_PATH: &str = "/run/z13helper/z13helperd.sock";
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
const MAX_ACTIVE_CLIENTS: usize = 64;
const MAX_SUBSCRIBERS: usize = 64;
const CONTROLLER_CAPTURE_LEASE: Duration = Duration::from_secs(3);

struct Subscriber {
    events: Vec<String>,
    stream: UnixStream,
    count: Arc<AtomicUsize>,
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    Active,
    Suspended,
    ShuttingDown,
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
    subscriber_count: Arc<AtomicUsize>,
    controller_capture_deadline: Arc<Mutex<Option<Instant>>>,
    lifecycle: Arc<Mutex<Lifecycle>>,
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
    let dispatch = {
        let lifecycle = lifecycle.lock().unwrap();
        if *lifecycle != Lifecycle::Active {
            write_response(
                &mut stream,
                &failure(ErrorCode::Rejected, "daemon is suspended or shutting down"),
            )?;
            return Ok(());
        }
        let dispatch = handle_line(&mut backend.lock().unwrap(), line.trim_end());
        if let Dispatch::ControllerCapture(enabled) = &dispatch {
            *controller_capture_deadline.lock().unwrap() =
                enabled.then(|| Instant::now() + CONTROLLER_CAPTURE_LEASE);
        }
        dispatch
    };
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
            if !reserve_subscriber(&subscriber_count) {
                write_response(
                    &mut stream,
                    &failure(ErrorCode::Rejected, "too many active subscriptions"),
                )?;
                return Ok(());
            }
            if let Err(error) = write_response(&mut stream, &WireResponse::success()) {
                subscriber_count.fetch_sub(1, Ordering::AcqRel);
                return Err(error);
            }
            stream.set_read_timeout(None)?;
            if let Err(error) = stream.set_nonblocking(true) {
                subscriber_count.fetch_sub(1, Ordering::AcqRel);
                return Err(error.into());
            }
            subscribers.lock().unwrap().push(Subscriber {
                events,
                stream,
                count: subscriber_count,
            });
        }
        Dispatch::ControllerCapture(_) => {
            write_response(&mut stream, &WireResponse::success())?;
        }
    }
    Ok(())
}

fn reserve_subscriber(count: &AtomicUsize) -> bool {
    count
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < MAX_SUBSCRIBERS).then_some(current + 1)
        })
        .is_ok()
}

fn subscriber_alive(stream: &UnixStream) -> bool {
    let mut byte = [0_u8; 1];
    // SAFETY: the stream owns this descriptor and `byte` is valid for one byte.
    match unsafe {
        libc::recv(
            stream.as_raw_fd(),
            byte.as_mut_ptr().cast(),
            byte.len(),
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    } {
        0 => false,
        result if result > 0 => true,
        _ => std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock,
    }
}

fn prune_subscribers(subscribers: &Arc<Mutex<Vec<Subscriber>>>) {
    subscribers
        .lock()
        .unwrap()
        .retain(|subscriber| subscriber_alive(&subscriber.stream));
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
    let mut body = serde_json::to_vec(&response).unwrap_or_default();
    body.push(b'\n');
    subscribers.lock().unwrap().retain_mut(|subscriber| {
        if !subscriber
            .events
            .iter()
            .any(|wanted| wanted == kind.as_str())
        {
            return true;
        }
        match subscriber.stream.write(&body) {
            Ok(written) => written == body.len(),
            Err(_) => false,
        }
    });
}

fn reserve_client_slot(active: &AtomicUsize) -> bool {
    active
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < MAX_ACTIVE_CLIENTS).then_some(current + 1)
        })
        .is_ok()
}

struct ClientSlot(Arc<AtomicUsize>);

impl Drop for ClientSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn lifecycle_is_active(lifecycle: &Arc<Mutex<Lifecycle>>) -> bool {
    *lifecycle.lock().unwrap() == Lifecycle::Active
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
    let subscriber_count = Arc::new(AtomicUsize::new(0));
    let active_clients = Arc::new(AtomicUsize::new(0));
    let terminate = Arc::new(AtomicBool::new(false));
    let lifecycle = Arc::new(Mutex::new(Lifecycle::Active));
    let controller_capture_deadline = Arc::new(Mutex::new(None::<Instant>));
    for signal in [SIGINT, SIGTERM, SIGHUP] {
        signal_hook::flag::register(signal, Arc::clone(&terminate))?;
    }

    let (event_tx, event_rx) = mpsc::channel();
    let button_watcher = spawn_button_watcher(event_tx, Arc::clone(&terminate));
    let (controller_tx, controller_rx) = mpsc::channel();
    let controller_capture = match spawn_controller_watcher(controller_tx, Arc::clone(&terminate)) {
        Ok(handle) => handle,
        Err(error) => {
            terminate.store(true, Ordering::Relaxed);
            let _ = button_watcher.join();
            return Err(error.into());
        }
    };
    let (sleep_tx, sleep_rx) = mpsc::channel();
    spawn_resume_watcher(sleep_tx, Arc::clone(&terminate));
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
                    if !reserve_client_slot(&active_clients) {
                        tracing::warn!("active client limit reached; rejecting connection");
                        continue;
                    }
                    let backend = Arc::clone(&backend);
                    let subscribers = Arc::clone(&subscribers);
                    let subscriber_count = Arc::clone(&subscriber_count);
                    let active_clients = Arc::clone(&active_clients);
                    let controller_capture_deadline = Arc::clone(&controller_capture_deadline);
                    let lifecycle = Arc::clone(&lifecycle);
                    thread::spawn(move || {
                        let _slot = ClientSlot(active_clients);
                        if let Err(error) = handle_client(
                            stream,
                            backend,
                            subscribers,
                            subscriber_count,
                            controller_capture_deadline,
                            lifecycle,
                        ) {
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
        prune_subscribers(&subscribers);
        if lifecycle_is_active(&lifecycle) && Instant::now() >= next_direct_tick {
            backend.lock().unwrap().tick();
            next_direct_tick = Instant::now() + DIRECT_TICK_INTERVAL;
        }
        if lifecycle_is_active(&lifecycle) && Instant::now() >= next_observe {
            backend.lock().unwrap().observe();
            next_observe = Instant::now() + Duration::from_secs(1);
        }
        if lifecycle_is_active(&lifecycle) && Instant::now() >= next_hotplug {
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
                SleepEvent::Sleeping => {
                    let suspend = {
                        let mut lifecycle = lifecycle.lock().unwrap();
                        if *lifecycle == Lifecycle::Active {
                            *lifecycle = Lifecycle::Suspended;
                            true
                        } else {
                            false
                        }
                    };
                    if suspend {
                        if controller_capture.set_enabled(false).is_ok() {
                            controller_capture_enabled = false;
                            steam_blocker.unblock();
                        } else {
                            tracing::warn!(
                                "controller release was not acknowledged before suspend"
                            );
                        }
                        backend.lock().unwrap().shutdown();
                    }
                }
                SleepEvent::Resumed => {
                    let mut lifecycle_state = lifecycle.lock().unwrap();
                    if *lifecycle_state != Lifecycle::Suspended {
                        continue;
                    }
                    let power_source_changed = backend.lock().unwrap().restore_volatile();
                    *lifecycle_state = Lifecycle::Active;
                    drop(lifecycle_state);
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
                        if controller_capture.set_enabled(false).is_ok() {
                            steam_blocker.unblock();
                        } else {
                            tracing::warn!(
                                "controller release was not acknowledged after capture setup failure"
                            );
                        }
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

    *lifecycle.lock().unwrap() = Lifecycle::ShuttingDown;
    let released = controller_capture.set_enabled(false).is_ok();
    if released {
        steam_blocker.unblock();
    }
    controller_capture.stop();
    if !released {
        steam_blocker.unblock();
    }
    backend.lock().unwrap().shutdown();
    let _ = button_watcher.join();
    drop(listener);
    let _ = fs::remove_file(SOCKET_PATH);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Lifecycle, MAX_ACTIVE_CLIENTS, MAX_SUBSCRIBERS, lifecycle_is_active, reserve_client_slot,
        reserve_subscriber, subscriber_alive,
    };
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    #[test]
    fn client_and_subscriber_budgets_are_hard_caps() {
        let clients = AtomicUsize::new(MAX_ACTIVE_CLIENTS);
        let subscribers = AtomicUsize::new(MAX_SUBSCRIBERS);
        assert!(!reserve_client_slot(&clients));
        assert!(!reserve_subscriber(&subscribers));
    }

    #[test]
    fn lifecycle_only_allows_hardware_requests_when_active() {
        let lifecycle = Arc::new(Mutex::new(Lifecycle::Suspended));
        assert!(!lifecycle_is_active(&lifecycle));
        *lifecycle.lock().unwrap() = Lifecycle::Active;
        assert!(lifecycle_is_active(&lifecycle));
        *lifecycle.lock().unwrap() = Lifecycle::ShuttingDown;
        assert!(!lifecycle_is_active(&lifecycle));
    }

    #[test]
    fn closed_idle_subscriber_is_detected_without_blocking() {
        let (peer, stream) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        assert!(subscriber_alive(&stream));
        drop(peer);
        assert!(!subscriber_alive(&stream));
    }

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
