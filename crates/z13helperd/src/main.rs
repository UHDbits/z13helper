use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
use z13helper_core::error::ErrorCode;
use z13helper_core::protocol::{
    ControllerAction, DaemonEvent, DaemonEventKind, EventTopic, MAX_FRAME_BYTES, PROTOCOL_VERSION,
    RequestOutcome, WireResponse,
};
use z13helperd::ec::{EcMailbox, LinuxPortIo};
use z13helperd::executor::{
    BackendExecutor, CommandSubmission, ExecutorEvent, OutcomeKey, PeerLiveness,
    REQUEST_QUEUE_DEADLINE,
};
use z13helperd::input::{
    CaptureStatus, ControllerCaptureHandle, spawn_button_watcher, spawn_controller_watcher,
};
use z13helperd::protocol::{Dispatch, Effect, Reply, failure, parse_line};
use z13helperd::resume::{SleepEvent, spawn_resume_watcher};
use z13helperd::service::Controller;
use z13helperd::steam::SteamBlocker;

mod logging;

const EXPECTED_MODEL: &str = "GZ302EA";
const DMI_PRODUCT_NAME: &str = "/sys/class/dmi/id/product_name";
const SOCKET_PATH: &str = "/run/z13helper/z13helperd.sock";
const LOCK_PATH: &str = "/run/z13helper/z13helperd.lock";
const MAX_ACTIVE_CLIENTS: usize = 64;
const MAX_ACCEPTS_PER_PUMP: usize = 8;
const BACKEND_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_SUBSCRIBERS: usize = 64;
const BUTTON_EVENT_CAPACITY: usize = 8;
const CONTROLLER_ACTION_CAPACITY: usize = 32;
const SLEEP_EVENT_CAPACITY: usize = 4;
const CONTROLLER_CAPTURE_LEASE: Duration = Duration::from_secs(3);
const CAPTURE_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const CAPTURE_RETRY_INTERVAL: Duration = Duration::from_millis(250);

struct Subscriber {
    events: Vec<EventTopic>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapturePhase {
    Released,
    Enabling { sequence: u64, deadline: Instant },
    Captured { sequence: u64 },
    Partial { sequence: u64, requested: bool },
    ReleasePending { sequence: u64 },
    Releasing { sequence: u64, deadline: Instant },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BpfAction {
    None,
    Block,
    Unblock,
}

struct CaptureCoordinator {
    desired: bool,
    phase: CapturePhase,
    bpf_blocked: bool,
    retry_at: Option<Instant>,
}

impl CaptureCoordinator {
    fn new() -> Self {
        Self {
            desired: false,
            phase: CapturePhase::Released,
            bpf_blocked: false,
            retry_at: None,
        }
    }

    fn set_desired(&mut self, desired: bool, now: Instant) -> BpfAction {
        if self.desired == desired {
            return BpfAction::None;
        }
        self.desired = desired;
        self.retry_at = desired.then_some(now);
        if !desired && self.bpf_blocked {
            self.bpf_blocked = false;
            BpfAction::Unblock
        } else {
            BpfAction::None
        }
    }

    fn request_needed(&self, now: Instant) -> Option<bool> {
        if self.retry_at.is_some_and(|retry_at| retry_at > now) {
            return None;
        }
        match (self.desired, self.phase) {
            (true, CapturePhase::Released) => Some(true),
            (false, CapturePhase::Enabling { .. })
            | (false, CapturePhase::Captured { .. })
            | (false, CapturePhase::Partial { .. })
            | (false, CapturePhase::ReleasePending { .. }) => Some(false),
            _ => None,
        }
    }

    fn request_sent(&mut self, enabled: bool, sequence: u64, now: Instant) {
        self.retry_at = None;
        self.phase = if enabled {
            CapturePhase::Enabling {
                sequence,
                deadline: now + CAPTURE_ACK_TIMEOUT,
            }
        } else {
            CapturePhase::Releasing {
                sequence,
                deadline: now + CAPTURE_ACK_TIMEOUT,
            }
        };
    }

    fn request_failed(&mut self, enabled: bool, now: Instant) {
        self.retry_at = Some(now + CAPTURE_RETRY_INTERVAL);
        if !enabled {
            let sequence = match self.phase {
                CapturePhase::Enabling { sequence, .. }
                | CapturePhase::Captured { sequence }
                | CapturePhase::Partial { sequence, .. }
                | CapturePhase::ReleasePending { sequence, .. }
                | CapturePhase::Releasing { sequence, .. } => sequence,
                CapturePhase::Released => return,
            };
            self.phase = CapturePhase::ReleasePending { sequence };
        }
    }

    fn expire(&mut self, now: Instant) -> Option<bool> {
        match self.phase {
            CapturePhase::Enabling { deadline, .. } if deadline <= now => {
                tracing::warn!("controller capture acknowledgement timed out; releasing");
                Some(false)
            }
            CapturePhase::Releasing { deadline, .. } if deadline <= now => {
                tracing::warn!(
                    "controller release acknowledgement timed out; retaining unblocked state"
                );
                let sequence = match self.phase {
                    CapturePhase::Releasing { sequence, .. } => sequence,
                    _ => unreachable!(),
                };
                self.phase = CapturePhase::ReleasePending { sequence };
                self.retry_at = Some(now);
                Some(false)
            }
            _ => None,
        }
    }

    fn status(&mut self, status: CaptureStatus, now: Instant) -> BpfAction {
        let (sequence, requested) = match status {
            CaptureStatus::Captured { sequence, .. } => (sequence, true),
            CaptureStatus::Released { sequence, .. } => (sequence, false),
            CaptureStatus::PartialGrab {
                sequence,
                requested,
                ..
            } => (sequence, requested),
        };
        let current_sequence = match self.phase {
            CapturePhase::Enabling { sequence, .. }
            | CapturePhase::Captured { sequence }
            | CapturePhase::Partial { sequence, .. }
            | CapturePhase::ReleasePending { sequence, .. }
            | CapturePhase::Releasing { sequence, .. } => Some(sequence),
            CapturePhase::Released => None,
        };
        if current_sequence != Some(sequence) {
            return BpfAction::None;
        }

        match status {
            CaptureStatus::Captured { grabbed, total, .. }
                if self.desired && total > 0 && grabbed == total =>
            {
                self.phase = CapturePhase::Captured { sequence };
                if !self.bpf_blocked {
                    self.bpf_blocked = true;
                    BpfAction::Block
                } else {
                    BpfAction::None
                }
            }
            CaptureStatus::Released { grabbed: 0, .. } => {
                self.phase = CapturePhase::Released;
                self.retry_at = self.desired.then_some(now);
                if self.bpf_blocked {
                    self.bpf_blocked = false;
                    BpfAction::Unblock
                } else {
                    BpfAction::None
                }
            }
            CaptureStatus::PartialGrab { .. }
            | CaptureStatus::Captured { .. }
            | CaptureStatus::Released { .. } => {
                self.phase = CapturePhase::Partial {
                    sequence,
                    requested,
                };
                self.retry_at = (!self.desired).then_some(now + CAPTURE_RETRY_INTERVAL);
                if self.bpf_blocked {
                    self.bpf_blocked = false;
                    BpfAction::Unblock
                } else {
                    BpfAction::None
                }
            }
        }
    }
}

fn issue_capture_request(
    capture: &mut CaptureCoordinator,
    handle: &ControllerCaptureHandle,
    enabled: bool,
    now: Instant,
) {
    match handle.request_capture(enabled) {
        Ok(sequence) => capture.request_sent(enabled, sequence, now),
        Err(error) => {
            capture.request_failed(enabled, now);
            tracing::warn!(%error, enabled, "could not queue controller capture request");
        }
    }
}

fn apply_bpf_action(action: BpfAction, steam_blocker: &mut SteamBlocker) {
    match action {
        BpfAction::None => {}
        BpfAction::Block => steam_blocker.block(),
        BpfAction::Unblock => steam_blocker.unblock(),
    }
}

fn drive_capture(
    capture: &mut CaptureCoordinator,
    handle: &ControllerCaptureHandle,
    steam_blocker: &mut SteamBlocker,
    desired: bool,
    now: Instant,
    next_steam_retry: &mut Instant,
) {
    // Queue release before applying the corresponding BPF cleanup. The
    // request is nonblocking; input ownership is reconciled by its own thread.
    let desired_action = capture.set_desired(desired, now);
    if let Some(enabled) = capture.expire(now) {
        issue_capture_request(capture, handle, enabled, now);
    }
    if let Some(enabled) = capture.request_needed(now) {
        issue_capture_request(capture, handle, enabled, now);
        if !enabled {
            apply_bpf_action(BpfAction::Unblock, steam_blocker);
        }
    }
    apply_bpf_action(desired_action, steam_blocker);

    loop {
        match handle.poll_status() {
            Ok(Some(status)) => {
                apply_bpf_action(capture.status(status, now), steam_blocker);
                if capture.bpf_blocked {
                    *next_steam_retry = now + Duration::from_secs(2);
                }
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "could not read controller capture status");
                break;
            }
        }
    }

    if desired && capture.bpf_blocked && now >= *next_steam_retry {
        steam_blocker.block();
        *next_steam_retry = now + Duration::from_secs(2);
    }
}

struct SingletonLock {
    _file: File,
}

impl SingletonLock {
    fn acquire(path: &Path) -> Result<Self> {
        let parent = path.parent().context("daemon lock has no parent")?;
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("open daemon lock {}", path.display()))?;
        // SAFETY: `file` owns a valid open file descriptor for the lock file.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                bail!("another z13helperd instance owns {}", path.display());
            }
            return Err(error).with_context(|| format!("acquire daemon lock {}", path.display()));
        }
        Ok(Self { _file: file })
    }
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

fn bind_socket(path: &Path, _singleton: &SingletonLock) -> Result<UnixListener> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_socket() {
            bail!("refusing to replace non-socket {}", path.display());
        }
        fs::remove_file(path).with_context(|| format!("remove stale socket {}", path.display()))?;
    }
    let parent = path.parent().context("daemon socket has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    let identity = socket_identity(path)?;
    if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o660)) {
        drop(listener);
        remove_owned_socket(path, identity);
        return Err(error).with_context(|| format!("chmod {}", path.display()));
    }
    if let Err(error) = listener.set_nonblocking(true) {
        drop(listener);
        remove_owned_socket(path, identity);
        return Err(error.into());
    }
    Ok(listener)
}

#[derive(Clone, Copy)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

struct OwnedSocket {
    path: PathBuf,
    identity: SocketIdentity,
}

fn socket_identity(path: &Path) -> Result<SocketIdentity> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat bound socket {}", path.display()))?;
    if !metadata.file_type().is_socket() {
        bail!("bound path {} is not a socket", path.display());
    }
    Ok(SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn remove_owned_socket(path: &Path, identity: SocketIdentity) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_socket()
        && metadata.dev() == identity.device
        && metadata.ino() == identity.inode
    {
        let _ = fs::remove_file(path);
    }
}

impl OwnedSocket {
    fn from_bound(path: &Path) -> Result<Self> {
        let identity = socket_identity(path)?;
        Ok(Self {
            path: path.to_owned(),
            identity,
        })
    }
}

impl Drop for OwnedSocket {
    fn drop(&mut self) {
        remove_owned_socket(&self.path, self.identity);
    }
}

fn encode_frame(response: &WireResponse) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(response)?;
    if body.len() + 1 > MAX_FRAME_BYTES {
        bail!("encoded response exceeds {MAX_FRAME_BYTES} bytes");
    }
    let mut frame = body;
    frame.push(b'\n');
    Ok(frame)
}

fn write_response(stream: &mut UnixStream, response: &WireResponse) -> Result<()> {
    stream.write_all(&encode_frame(response)?)?;
    Ok(())
}

fn read_request_frame(reader: &mut impl BufRead) -> Result<Option<String>> {
    let mut line = String::new();
    let read = reader
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') || line.len() > MAX_FRAME_BYTES {
        bail!("request exceeds {MAX_FRAME_BYTES} bytes or was not newline terminated");
    }
    Ok(Some(line))
}

fn handle_client(
    mut stream: UnixStream,
    executor: Arc<BackendExecutor>,
    subscribers: Arc<Mutex<Vec<Subscriber>>>,
    subscriber_count: Arc<AtomicUsize>,
    controller_capture_deadline: Arc<Mutex<Option<Instant>>>,
    lifecycle: Arc<Mutex<Lifecycle>>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(&stream);
    let Some(line) = read_request_frame(&mut reader)? else {
        return Ok(());
    };
    let dispatch = parse_line(line.trim_end());
    let active = {
        let lifecycle = lifecycle.lock().unwrap();
        *lifecycle == Lifecycle::Active
    };
    if !active {
        let request_id = match &dispatch {
            Dispatch::Reply(reply) => reply.response.request_id,
            Dispatch::Subscribe { request_id, .. }
            | Dispatch::OutcomeQuery { request_id, .. }
            | Dispatch::Command { request_id, .. } => Some(*request_id),
        };
        write_response(
            &mut stream,
            &failure(
                request_id,
                ErrorCode::Rejected,
                "daemon is suspended or shutting down",
            ),
        )?;
        return Ok(());
    }
    match dispatch {
        Dispatch::Reply(reply) => {
            let Reply {
                client_id,
                response,
                effects,
            } = *reply;
            if response.ok
                && let Some(client_id) = client_id
                && let Some(request_id) = response.request_id
            {
                let key = OutcomeKey::new(client_id, request_id);
                if let Some(existing) = executor.begin_immediate(key) {
                    write_response(&mut stream, &existing)?;
                    return Ok(());
                }
                commit_effects(&effects, &subscribers, &controller_capture_deadline);
                executor.complete_immediate(key, response.clone());
            } else {
                commit_effects(&effects, &subscribers, &controller_capture_deadline);
            }
            write_response(&mut stream, &response)?;
        }
        Dispatch::Subscribe {
            client_id: _,
            request_id,
            events,
        } => {
            if !reserve_subscriber(&subscriber_count) {
                write_response(
                    &mut stream,
                    &failure(
                        Some(request_id),
                        ErrorCode::Rejected,
                        "too many active subscriptions",
                    ),
                )?;
                return Ok(());
            }
            if let Err(error) = write_response(&mut stream, &WireResponse::success(request_id)) {
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
        Dispatch::OutcomeQuery {
            request_id,
            target_client_id,
            target,
        } => {
            let response = executor
                .lookup_outcome(OutcomeKey::new(target_client_id, target), request_id)
                .unwrap_or_else(|| {
                    failure(
                        Some(request_id),
                        ErrorCode::Rejected,
                        "request outcome is unknown or has expired from the bounded cache",
                    )
                });
            write_response(&mut stream, &response)?;
        }
        Dispatch::Command {
            client_id,
            request_id,
            command,
            effects,
        } => {
            let peer = PeerLiveness::from_stream(&stream)
                .map_err(|error| anyhow::anyhow!("duplicate client stream: {error}"))?;
            let submission = match executor.submit_command(
                OutcomeKey::new(client_id, request_id),
                *command,
                effects,
                peer,
                Instant::now() + REQUEST_QUEUE_DEADLINE,
            ) {
                Ok(receiver) => receiver,
                Err(error) => {
                    write_response(
                        &mut stream,
                        &failure(Some(request_id), ErrorCode::Rejected, error.message()),
                    )?;
                    return Ok(());
                }
            };
            let receiver = match submission {
                CommandSubmission::Existing(response) => {
                    write_response(&mut stream, &response)?;
                    return Ok(());
                }
                CommandSubmission::Accepted(receiver) => receiver,
            };
            write_response(
                &mut stream,
                &WireResponse::progress(request_id, RequestOutcome::Queued),
            )?;
            loop {
                let response = match receiver.recv_timeout(BACKEND_RESPONSE_TIMEOUT) {
                    Ok(response) => response,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        tracing::warn!(
                            request_id = request_id.get(),
                            "backend request response timed out; outcome is unknown"
                        );
                        return Ok(());
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        tracing::warn!(
                            request_id = request_id.get(),
                            "backend executor stopped before completing request"
                        );
                        return Ok(());
                    }
                };
                let terminal = matches!(
                    response.outcome,
                    RequestOutcome::Completed
                        | RequestOutcome::Expired
                        | RequestOutcome::Disconnected
                );
                // Effects are published by the owner before the completed
                // response is sent. A failed write cannot hide a commit.
                write_response(&mut stream, &response)?;
                if terminal {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn commit_effects(
    effects: &[Effect],
    subscribers: &Arc<Mutex<Vec<Subscriber>>>,
    controller_capture_deadline: &Arc<Mutex<Option<Instant>>>,
) {
    for effect in effects {
        match *effect {
            Effect::StateChanged => {
                broadcast(subscribers, DaemonEventKind::StateChanged, None, None);
            }
            Effect::ControllerCapture(enabled) => {
                *controller_capture_deadline.lock().unwrap() =
                    enabled.then(|| Instant::now() + CONTROLLER_CAPTURE_LEASE);
            }
        }
    }
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
    kind: EventTopic,
    action: Option<ControllerAction>,
    on_battery: Option<bool>,
) {
    let response = WireResponse {
        version: PROTOCOL_VERSION,
        request_id: None,
        outcome: RequestOutcome::Completed,
        outcome_client_id: None,
        outcome_request_id: None,
        ok: true,
        state: None,
        apply: None,
        probe: None,
        factory_fan_curves: None,
        event: Some(DaemonEvent {
            kind,
            action,
            on_battery,
        }),
        error: None,
    };
    let body = match encode_frame(&response) {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(%error, "event response exceeds protocol frame limit");
            return;
        }
    };
    subscribers.lock().unwrap().retain_mut(|subscriber| {
        if !subscriber.events.contains(&kind) {
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

fn reconcile_failed_suspend(lifecycle: &Arc<Mutex<Lifecycle>>) {
    let mut lifecycle = lifecycle.lock().unwrap();
    if *lifecycle == Lifecycle::Suspended {
        *lifecycle = Lifecycle::Active;
    }
}

fn release_ec_only(_singleton: &SingletonLock) -> Result<()> {
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
    let singleton = SingletonLock::acquire(Path::new(LOCK_PATH))?;
    if std::env::args().any(|argument| argument == "--release-ec") {
        return release_ec_only(&singleton);
    }
    verify_model()?;
    let listener = bind_socket(Path::new(SOCKET_PATH), &singleton)?;
    let socket_owner = match OwnedSocket::from_bound(Path::new(SOCKET_PATH)) {
        Ok(owner) => owner,
        Err(error) => {
            drop(listener);
            return Err(error);
        }
    };
    let executor = Arc::new(match BackendExecutor::start() {
        Ok(executor) => executor,
        Err(error) => {
            drop(listener);
            drop(socket_owner);
            return Err(anyhow::Error::msg(error));
        }
    });
    let subscribers = Arc::new(Mutex::new(Vec::new()));
    let subscriber_count = Arc::new(AtomicUsize::new(0));
    let active_clients = Arc::new(AtomicUsize::new(0));
    let terminate = Arc::new(AtomicBool::new(false));
    let lifecycle = Arc::new(Mutex::new(Lifecycle::Active));
    let controller_capture_deadline = Arc::new(Mutex::new(None::<Instant>));
    for signal in [SIGINT, SIGTERM, SIGHUP] {
        signal_hook::flag::register(signal, Arc::clone(&terminate))?;
    }

    let (event_tx, event_rx) = mpsc::sync_channel(BUTTON_EVENT_CAPACITY);
    let button_watcher = spawn_button_watcher(event_tx, Arc::clone(&terminate));
    let (controller_tx, controller_rx) = mpsc::sync_channel(CONTROLLER_ACTION_CAPACITY);
    let controller_capture = match spawn_controller_watcher(controller_tx, Arc::clone(&terminate)) {
        Ok(handle) => handle,
        Err(error) => {
            terminate.store(true, Ordering::Relaxed);
            let _ = button_watcher.join();
            executor.shutdown();
            return Err(error.into());
        }
    };
    let (sleep_tx, sleep_rx) = mpsc::sync_channel(SLEEP_EVENT_CAPACITY);
    spawn_resume_watcher(sleep_tx, Arc::clone(&terminate));
    let mut next_observe = Instant::now();
    let mut next_hotplug = Instant::now();
    let mut next_steam_retry = Instant::now();
    let mut capture = CaptureCoordinator::new();
    let mut steam_blocker = SteamBlocker::new();
    let mut resume_pending = false;
    tracing::info!(
        socket = SOCKET_PATH,
        protocol = PROTOCOL_VERSION,
        "z13helperd ready"
    );

    while !terminate.load(Ordering::Relaxed) {
        for _ in 0..MAX_ACCEPTS_PER_PUMP {
            match listener.accept() {
                Ok((stream, _)) => {
                    if !reserve_client_slot(&active_clients) {
                        tracing::warn!("active client limit reached; rejecting connection");
                        let mut stream = stream;
                        let _ = write_response(
                            &mut stream,
                            &failure(None, ErrorCode::Rejected, "too many active clients"),
                        );
                        continue;
                    }
                    let executor = Arc::clone(&executor);
                    let subscribers = Arc::clone(&subscribers);
                    let subscriber_count = Arc::clone(&subscriber_count);
                    let thread_active_clients = Arc::clone(&active_clients);
                    let controller_capture_deadline = Arc::clone(&controller_capture_deadline);
                    let lifecycle = Arc::clone(&lifecycle);
                    let spawn = thread::Builder::new()
                        .name("z13helper-client".into())
                        .spawn(move || {
                            let _slot = ClientSlot(thread_active_clients);
                            if let Err(error) = handle_client(
                                stream,
                                executor,
                                subscribers,
                                subscriber_count,
                                controller_capture_deadline,
                                lifecycle,
                            ) {
                                tracing::warn!(%error, "client request failed");
                            }
                        });
                    if let Err(error) = spawn {
                        active_clients.fetch_sub(1, Ordering::AcqRel);
                        tracing::warn!(%error, "could not start bounded client handler");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    tracing::error!(%error, "socket accept failed");
                    break;
                }
            }
        }
        prune_subscribers(&subscribers);
        if lifecycle_is_active(&lifecycle) && Instant::now() >= next_observe {
            let _ = executor.try_observe();
            next_observe = Instant::now() + Duration::from_secs(1);
        }
        if lifecycle_is_active(&lifecycle) && Instant::now() >= next_hotplug {
            let _ = executor.try_hotplug();
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
                        *controller_capture_deadline.lock().unwrap() = None;
                        drive_capture(
                            &mut capture,
                            &controller_capture,
                            &mut steam_blocker,
                            false,
                            Instant::now(),
                            &mut next_steam_retry,
                        );
                        if let Err(error) = executor.begin_suspend() {
                            tracing::warn!(?error, "could not enqueue suspend barrier");
                            // Admission closes before this call so clients
                            // cannot observe a half-suspended state. If the
                            // barrier itself cannot be admitted, reconcile
                            // the external lifecycle state with the active
                            // backend immediately.
                            reconcile_failed_suspend(&lifecycle);
                        }
                    }
                }
                SleepEvent::Resumed => {
                    let lifecycle_state = lifecycle.lock().unwrap();
                    if *lifecycle_state != Lifecycle::Suspended {
                        continue;
                    }
                    drop(lifecycle_state);
                    match executor.begin_resume() {
                        Ok(()) => {}
                        Err(error) => {
                            resume_pending = true;
                            tracing::debug!(?error, "resume waits for suspend barrier");
                        }
                    }
                }
            }
        }
        for event in executor.drain_events() {
            match event {
                ExecutorEvent::StateChanged => {
                    broadcast(&subscribers, DaemonEventKind::StateChanged, None, None);
                }
                ExecutorEvent::Suspended => {
                    if resume_pending {
                        resume_pending = false;
                        if let Err(error) = executor.begin_resume() {
                            tracing::warn!(?error, "could not enqueue deferred resume barrier");
                        }
                    }
                }
                ExecutorEvent::Resumed(on_battery) => {
                    *lifecycle.lock().unwrap() = Lifecycle::Active;
                    if let Some(on_battery) = on_battery {
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
        let capture_requested = lifecycle_is_active(&lifecycle)
            && controller_capture_deadline
                .lock()
                .unwrap()
                .is_some_and(|deadline| deadline > Instant::now());
        drive_capture(
            &mut capture,
            &controller_capture,
            &mut steam_blocker,
            capture_requested,
            Instant::now(),
            &mut next_steam_retry,
        );
        thread::sleep(Duration::from_millis(25));
    }

    *lifecycle.lock().unwrap() = Lifecycle::ShuttingDown;
    drive_capture(
        &mut capture,
        &controller_capture,
        &mut steam_blocker,
        false,
        Instant::now(),
        &mut next_steam_retry,
    );
    controller_capture.stop();
    // The input thread also attempts an idempotent ungrab on exit. Clearing
    // BPF again is safe and covers a failed/missing asynchronous status.
    steam_blocker.unblock();
    executor.shutdown();
    let _ = button_watcher.join();
    drop(listener);
    drop(socket_owner);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BACKEND_RESPONSE_TIMEOUT, BpfAction, CaptureCoordinator, CapturePhase, CaptureStatus,
        Lifecycle, MAX_ACTIVE_CLIENTS, MAX_FRAME_BYTES, MAX_SUBSCRIBERS, OwnedSocket,
        SingletonLock, bind_socket, encode_frame, lifecycle_is_active, read_request_frame,
        reconcile_failed_suspend, reserve_client_slot, reserve_subscriber, subscriber_alive,
    };
    use std::fs;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
    use z13helper_core::error::ErrorCode;
    use z13helperd::protocol::failure;

    static NEXT_TEST_ID: AtomicUsize = AtomicUsize::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let counter = NEXT_TEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "z13helperd-main-test-{}-{timestamp}-{counter}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn client_and_subscriber_budgets_are_hard_caps() {
        let clients = AtomicUsize::new(MAX_ACTIVE_CLIENTS);
        let subscribers = AtomicUsize::new(MAX_SUBSCRIBERS);
        assert!(!reserve_client_slot(&clients));
        assert!(!reserve_subscriber(&subscribers));
    }

    #[test]
    fn request_frame_boundary_is_exact_and_overflow_is_rejected() {
        let mut exact = vec![b'x'; MAX_FRAME_BYTES - 1];
        exact.push(b'\n');
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(exact));
        assert_eq!(
            read_request_frame(&mut reader).unwrap().unwrap().len(),
            MAX_FRAME_BYTES
        );

        let mut oversized = vec![b'x'; MAX_FRAME_BYTES];
        oversized.push(b'\n');
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(oversized));
        assert!(read_request_frame(&mut reader).is_err());

        let mut unterminated =
            std::io::BufReader::new(std::io::Cursor::new(vec![b'x'; MAX_FRAME_BYTES - 1]));
        assert!(read_request_frame(&mut unterminated).is_err());
    }

    #[test]
    fn encoded_response_boundary_is_exact_and_overflow_is_rejected() {
        let response = failure(None, ErrorCode::Rejected, "");
        let empty_len = serde_json::to_vec(&response).unwrap().len();
        let exact = failure(
            None,
            ErrorCode::Rejected,
            "x".repeat(MAX_FRAME_BYTES - 1 - empty_len),
        );
        assert_eq!(encode_frame(&exact).unwrap().len(), MAX_FRAME_BYTES);

        let oversized = failure(
            None,
            ErrorCode::Rejected,
            "x".repeat(MAX_FRAME_BYTES - empty_len),
        );
        assert!(encode_frame(&oversized).is_err());
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
    fn failed_suspend_admission_reconciles_external_state_to_active() {
        let lifecycle = Arc::new(Mutex::new(Lifecycle::Suspended));
        reconcile_failed_suspend(&lifecycle);
        assert_eq!(*lifecycle.lock().unwrap(), Lifecycle::Active);
    }

    #[test]
    fn backend_response_wait_is_bounded_and_outcome_unknown_is_allowed() {
        assert_eq!(BACKEND_RESPONSE_TIMEOUT, Duration::from_secs(10));
    }

    #[test]
    fn zero_device_capture_is_partial_and_never_authorizes_bpf() {
        let now = Instant::now();
        let mut capture = CaptureCoordinator::new();
        assert_eq!(capture.set_desired(true, now), BpfAction::None);
        capture.request_sent(true, 1, now);
        assert_eq!(
            capture.status(
                CaptureStatus::PartialGrab {
                    sequence: 1,
                    requested: true,
                    grabbed: 0,
                    total: 0,
                },
                now,
            ),
            BpfAction::None
        );
        assert_eq!(
            capture.phase,
            CapturePhase::Partial {
                sequence: 1,
                requested: true
            }
        );
        assert!(!capture.bpf_blocked);
    }

    #[test]
    fn release_timeout_and_send_failure_keep_retrying_until_explicit_release() {
        let now = Instant::now();
        let mut capture = CaptureCoordinator::new();
        capture.set_desired(true, now);
        capture.request_sent(true, 1, now);
        assert_eq!(
            capture.status(
                CaptureStatus::Captured {
                    sequence: 1,
                    grabbed: 1,
                    total: 1,
                },
                now,
            ),
            BpfAction::Block
        );
        assert_eq!(capture.set_desired(false, now), BpfAction::Unblock);
        capture.request_sent(false, 2, now);

        let timed_out = now + super::CAPTURE_ACK_TIMEOUT;
        assert_eq!(capture.expire(timed_out), Some(false));
        assert_eq!(capture.phase, CapturePhase::ReleasePending { sequence: 2 });
        capture.request_failed(false, timed_out);
        assert_eq!(capture.request_needed(timed_out), None);
        assert_eq!(
            capture.request_needed(timed_out + super::CAPTURE_RETRY_INTERVAL),
            Some(false)
        );
        assert_eq!(capture.phase, CapturePhase::ReleasePending { sequence: 2 });

        assert_eq!(
            capture.status(
                CaptureStatus::Released {
                    sequence: 2,
                    grabbed: 1,
                    total: 1,
                },
                timed_out + Duration::from_millis(1),
            ),
            BpfAction::None
        );
        assert_eq!(
            capture.request_needed(timed_out + Duration::from_secs(1),),
            Some(false)
        );

        assert_eq!(
            capture.status(
                CaptureStatus::Released {
                    sequence: 2,
                    grabbed: 0,
                    total: 1,
                },
                timed_out + Duration::from_secs(1),
            ),
            BpfAction::None
        );
        assert_eq!(capture.phase, CapturePhase::Released);
        assert!(!capture.bpf_blocked);
    }

    #[test]
    fn partial_release_status_schedules_another_ungrab_attempt() {
        let now = Instant::now();
        let mut capture = CaptureCoordinator::new();
        capture.set_desired(true, now);
        capture.request_sent(true, 1, now);
        capture.set_desired(false, now);
        capture.request_sent(false, 2, now);
        assert_eq!(
            capture.status(
                CaptureStatus::PartialGrab {
                    sequence: 2,
                    requested: false,
                    grabbed: 1,
                    total: 1,
                },
                now,
            ),
            BpfAction::None
        );
        assert_eq!(capture.request_needed(now), None);
        assert_eq!(
            capture.request_needed(now + super::CAPTURE_RETRY_INTERVAL),
            Some(false)
        );
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
    fn live_owner_keeps_socket_from_being_unlinked_by_contender() {
        let directory = TestDir::new();
        let lock_path = directory.path("z13helperd.lock");
        let socket_path = directory.path("z13helperd.sock");
        let owner = SingletonLock::acquire(&lock_path).unwrap();
        let listener = bind_socket(&socket_path, &owner).unwrap();

        assert!(SingletonLock::acquire(&lock_path).is_err());
        assert!(socket_path.exists());

        drop(listener);
        drop(owner);
    }

    #[test]
    fn stale_socket_is_replaced_after_lock_acquisition() {
        let directory = TestDir::new();
        let lock_path = directory.path("z13helperd.lock");
        let socket_path = directory.path("z13helperd.sock");
        let stale_listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        drop(stale_listener);
        assert!(socket_path.exists());

        let owner = SingletonLock::acquire(&lock_path).unwrap();
        let listener = bind_socket(&socket_path, &owner).unwrap();
        assert!(listener.local_addr().is_ok());

        drop(listener);
        drop(owner);
    }

    #[test]
    fn startup_cleanup_removes_our_socket_but_not_a_replacement() {
        let directory = TestDir::new();
        let socket_path = directory.path("z13helperd.sock");
        let owner = SingletonLock::acquire(&directory.path("z13helperd.lock")).unwrap();
        let listener = bind_socket(&socket_path, &owner).unwrap();
        let socket_owner = OwnedSocket::from_bound(&socket_path).unwrap();
        let original_identity = socket_owner.identity;
        drop(listener);
        assert!(socket_path.exists());
        drop(socket_owner);
        assert!(!socket_path.exists());

        let replacement = UnixListener::bind(&socket_path).unwrap();
        let stale_owner = OwnedSocket {
            path: socket_path.clone(),
            identity: original_identity,
        };
        drop(stale_owner);
        assert!(socket_path.exists());
        drop(replacement);
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
