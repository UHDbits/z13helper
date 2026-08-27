use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use z13helper_core::error::ErrorCode;
use z13helper_core::protocol::{ClientId, Command, RequestId, RequestOutcome, WireResponse};

use crate::backend::Backend;
use crate::protocol::{Effect, execute, failure};

/// The queue is deliberately small. Two slots are reserved for lifecycle
/// barriers so a request flood cannot prevent suspend or shutdown admission.
pub const BACKEND_QUEUE_CAPACITY: usize = 32;
pub const LIFECYCLE_RESERVE: usize = 2;
pub const REQUEST_QUEUE_DEADLINE: Duration = Duration::from_secs(10);

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_LIFECYCLE_EVENTS: usize = 8;
pub const MAX_RETAINED_OUTCOMES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OutcomeKey {
    pub client_id: ClientId,
    pub request_id: RequestId,
}

impl OutcomeKey {
    pub const fn new(client_id: ClientId, request_id: RequestId) -> Self {
        Self {
            client_id,
            request_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionMode {
    Active,
    BarrierPending,
    Suspended,
    ShuttingDown,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionError {
    Full,
    Suspended,
    ShuttingDown,
    BarrierPending,
    Stopped,
}

impl AdmissionError {
    pub fn message(self) -> &'static str {
        match self {
            Self::Full => "backend request queue is full",
            Self::Suspended => "daemon is suspended",
            Self::ShuttingDown => "daemon is shutting down",
            Self::BarrierPending => "daemon lifecycle barrier is pending",
            Self::Stopped => "backend executor is stopped",
        }
    }
}

#[derive(Debug)]
struct Admission {
    mode: AdmissionMode,
    queued: usize,
}

impl Admission {
    fn new() -> Self {
        Self {
            mode: AdmissionMode::Active,
            queued: 0,
        }
    }

    fn mode(&self) -> AdmissionMode {
        self.mode
    }

    fn release_queue_slot(&mut self) {
        self.queued = self.queued.saturating_sub(1);
    }

    fn admit_client(&mut self) -> Result<(), AdmissionError> {
        match self.mode {
            AdmissionMode::Active => {}
            AdmissionMode::Suspended => return Err(AdmissionError::Suspended),
            AdmissionMode::BarrierPending => return Err(AdmissionError::BarrierPending),
            AdmissionMode::ShuttingDown => return Err(AdmissionError::ShuttingDown),
            AdmissionMode::Stopped => return Err(AdmissionError::Stopped),
        }
        if self.queued >= BACKEND_QUEUE_CAPACITY - LIFECYCLE_RESERVE {
            return Err(AdmissionError::Full);
        }
        self.queued += 1;
        Ok(())
    }

    fn admit_maintenance(&mut self) -> Result<(), AdmissionError> {
        if self.mode != AdmissionMode::Active {
            return Err(match self.mode {
                AdmissionMode::Suspended => AdmissionError::Suspended,
                AdmissionMode::BarrierPending => AdmissionError::BarrierPending,
                AdmissionMode::ShuttingDown => AdmissionError::ShuttingDown,
                AdmissionMode::Stopped => AdmissionError::Stopped,
                AdmissionMode::Active => unreachable!(),
            });
        }
        if self.queued >= BACKEND_QUEUE_CAPACITY - LIFECYCLE_RESERVE {
            return Err(AdmissionError::Full);
        }
        self.queued += 1;
        Ok(())
    }

    fn begin_lifecycle(&mut self, operation: LifecycleOperation) -> Result<(), AdmissionError> {
        let allowed = match operation {
            LifecycleOperation::Suspend => self.mode == AdmissionMode::Active,
            LifecycleOperation::Resume => self.mode == AdmissionMode::Suspended,
            LifecycleOperation::Shutdown => {
                matches!(
                    self.mode,
                    AdmissionMode::Active
                        | AdmissionMode::BarrierPending
                        | AdmissionMode::Suspended
                )
            }
        };
        if !allowed {
            return Err(match self.mode {
                AdmissionMode::Suspended => AdmissionError::Suspended,
                AdmissionMode::BarrierPending => AdmissionError::BarrierPending,
                AdmissionMode::ShuttingDown => AdmissionError::ShuttingDown,
                AdmissionMode::Stopped => AdmissionError::Stopped,
                AdmissionMode::Active => AdmissionError::BarrierPending,
            });
        }
        if self.queued >= BACKEND_QUEUE_CAPACITY {
            return Err(AdmissionError::Full);
        }
        self.queued += 1;
        self.mode = match operation {
            LifecycleOperation::Suspend | LifecycleOperation::Resume => {
                AdmissionMode::BarrierPending
            }
            LifecycleOperation::Shutdown => AdmissionMode::ShuttingDown,
        };
        Ok(())
    }

    fn barrier_finished(&mut self, operation: LifecycleOperation) {
        self.mode = match operation {
            LifecycleOperation::Suspend => AdmissionMode::Suspended,
            LifecycleOperation::Resume => AdmissionMode::Active,
            LifecycleOperation::Shutdown => AdmissionMode::Stopped,
        };
    }
}

#[derive(Clone)]
pub struct PeerLiveness {
    stream: Arc<UnixStream>,
}

impl PeerLiveness {
    pub fn from_stream(stream: &UnixStream) -> io::Result<Self> {
        let stream = stream.try_clone()?;
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream: Arc::new(stream),
        })
    }

    /// A peer is considered alive when the duplicated socket has no EOF or
    /// terminal error. This is only used before a job starts; after hardware
    /// work begins, the transaction is never cancelled.
    pub fn is_alive(&self) -> bool {
        let mut byte = [0_u8; 1];
        // SAFETY: the duplicated stream owns this descriptor and `byte` is a
        // valid one-byte receive buffer.
        let result = unsafe {
            libc::recv(
                self.stream.as_raw_fd(),
                byte.as_mut_ptr().cast(),
                byte.len(),
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if result == 0 {
            return false;
        }
        if result > 0 {
            return true;
        }
        matches!(
            io::Error::last_os_error().kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
        )
    }
}

#[derive(Clone, Copy)]
enum LifecycleOperation {
    Suspend,
    Resume,
    Shutdown,
}

enum Operation {
    Command {
        key: OutcomeKey,
        command: Box<Command>,
        effects: Vec<Effect>,
        peer: PeerLiveness,
        deadline: Instant,
        reply: SyncSender<WireResponse>,
    },
    Observe,
    Hotplug,
    Lifecycle {
        operation: LifecycleOperation,
        ack: Option<SyncSender<()>>,
    },
}

struct Job {
    operation: Operation,
}

#[derive(Default)]
struct EventQueue {
    state_changed: bool,
    lifecycle: VecDeque<ExecutorEvent>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutorEvent {
    StateChanged,
    Suspended,
    Resumed(Option<bool>),
}

fn can_start(now: Instant, deadline: Instant, peer_alive: bool) -> bool {
    now < deadline && peer_alive
}

impl EventQueue {
    fn publish_effects(&mut self, effects: &[Effect]) {
        if effects.contains(&Effect::StateChanged) {
            // StateChanged is a snapshot invalidation, so coalescing is both
            // bounded and truthful; the next GetState reads the latest cache.
            self.state_changed = true;
        }
    }

    fn publish_lifecycle(&mut self, event: ExecutorEvent) {
        if self.lifecycle.len() == MAX_LIFECYCLE_EVENTS {
            // Lifecycle state is also held by AdmissionMode. Retaining the
            // newest bounded notification lets the pump resynchronize to the
            // authoritative mode instead of allowing unbounded growth.
            self.lifecycle.pop_front();
            tracing::warn!("lifecycle event queue full; dropped oldest notification");
        }
        self.lifecycle.push_back(event);
    }

    fn drain(&mut self) -> Vec<ExecutorEvent> {
        let mut events = Vec::with_capacity(self.lifecycle.len() + 1);
        if self.state_changed {
            self.state_changed = false;
            events.push(ExecutorEvent::StateChanged);
        }
        events.extend(self.lifecycle.drain(..));
        events
    }
}

pub struct BackendExecutor {
    sender: Mutex<Option<SyncSender<Job>>>,
    admission: Arc<Mutex<Admission>>,
    events: Arc<Mutex<EventQueue>>,
    outcomes: Arc<OutcomeStore>,
    owner: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone, Debug)]
struct StoredOutcome {
    outcome: RequestOutcome,
    response: Option<WireResponse>,
}

#[derive(Default)]
struct OutcomeStore {
    entries: Mutex<HashMap<OutcomeKey, StoredOutcome>>,
    completed: Mutex<VecDeque<OutcomeKey>>,
}

impl OutcomeStore {
    fn register(&self, key: OutcomeKey) -> Option<WireResponse> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(stored) = entries.get(&key) {
            return Some(snapshot(key, key.request_id, stored, false));
        }
        entries.insert(
            key,
            StoredOutcome {
                outcome: RequestOutcome::Queued,
                response: None,
            },
        );
        None
    }

    fn remove(&self, key: OutcomeKey) {
        self.entries.lock().unwrap().remove(&key);
    }

    fn set_started(&self, key: OutcomeKey) {
        if let Some(stored) = self.entries.lock().unwrap().get_mut(&key) {
            stored.outcome = RequestOutcome::Started;
        }
    }

    fn set_terminal(&self, key: OutcomeKey, outcome: RequestOutcome, response: WireResponse) {
        let mut entries = self.entries.lock().unwrap();
        entries.insert(
            key,
            StoredOutcome {
                outcome,
                response: Some(response),
            },
        );
        let mut completed = self.completed.lock().unwrap();
        completed.push_back(key);
        while completed.len() > MAX_RETAINED_OUTCOMES {
            if let Some(old) = completed.pop_front() {
                entries.remove(&old);
            }
        }
    }

    fn lookup(&self, target: OutcomeKey, correlation: RequestId) -> Option<WireResponse> {
        self.entries
            .lock()
            .unwrap()
            .get(&target)
            .map(|stored| snapshot(target, correlation, stored, true))
    }
}

fn snapshot(
    target: OutcomeKey,
    correlation: RequestId,
    stored: &StoredOutcome,
    query: bool,
) -> WireResponse {
    let mut response = stored
        .response
        .clone()
        .unwrap_or_else(|| WireResponse::progress(correlation, stored.outcome));
    response.version = z13helper_core::protocol::PROTOCOL_VERSION;
    response.request_id = Some(correlation);
    response.outcome = stored.outcome;
    response.outcome_client_id = query.then_some(target.client_id);
    response.outcome_request_id = query.then_some(target.request_id);
    if query {
        // The lookup itself succeeded. The original operation's failure is
        // retained in `error` for callers to inspect without turning a known
        // completed outcome into a transport error.
        response.ok = true;
    }
    response
}

pub enum CommandSubmission {
    Accepted(Receiver<WireResponse>),
    Existing(Box<WireResponse>),
}

impl BackendExecutor {
    /// Start the owner after the caller has acquired singleton ownership and
    /// bound its socket. Backend::start is the first hardware acquisition in
    /// this thread and never occurs on a client or daemon-pump thread.
    pub fn start() -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(BACKEND_QUEUE_CAPACITY);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let admission = Arc::new(Mutex::new(Admission::new()));
        let events = Arc::new(Mutex::new(EventQueue::default()));
        let outcomes = Arc::new(OutcomeStore::default());
        let worker_events = Arc::clone(&events);
        let worker_admission = Arc::clone(&admission);
        let worker_outcomes = Arc::clone(&outcomes);
        let owner = thread::Builder::new()
            .name("z13helper-backend".into())
            .spawn(move || match Backend::start() {
                Ok(mut backend) => {
                    let _ = ready_sender.send(Ok(()));
                    run_owner(
                        &mut backend,
                        receiver,
                        worker_admission,
                        worker_events,
                        worker_outcomes,
                    );
                }
                Err(error) => {
                    let _ = ready_sender.send(Err(error));
                }
            })
            .map_err(|error| error.to_string())?;
        match ready_receiver.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                sender: Mutex::new(Some(sender)),
                admission,
                events,
                outcomes,
                owner: Mutex::new(Some(owner)),
            }),
            Ok(Err(error)) => {
                let _ = owner.join();
                Err(error)
            }
            Err(error) => {
                // The backend may already have acquired hardware while the
                // handshake timed out. Disconnect its receiver and join; do
                // not detach a thread that owns hardware.
                drop(sender);
                let _ = owner.join();
                Err(format!("backend startup timed out: {error}"))
            }
        }
    }

    pub fn mode(&self) -> AdmissionMode {
        self.admission.lock().unwrap().mode()
    }

    pub fn submit_command(
        &self,
        key: OutcomeKey,
        command: Command,
        effects: Vec<Effect>,
        peer: PeerLiveness,
        deadline: Instant,
    ) -> Result<CommandSubmission, AdmissionError> {
        if let Some(existing) = self.outcomes.register(key) {
            return Ok(CommandSubmission::Existing(Box::new(existing)));
        }
        let (reply, receiver) = mpsc::sync_channel(2);
        let result = self.admit_client(Job {
            operation: Operation::Command {
                key,
                command: Box::new(command),
                effects,
                peer,
                deadline,
                reply,
            },
        });
        if result.is_err() {
            self.outcomes.remove(key);
        }
        result.map(|()| CommandSubmission::Accepted(receiver))
    }

    pub fn lookup_outcome(
        &self,
        target: OutcomeKey,
        correlation: RequestId,
    ) -> Option<WireResponse> {
        self.outcomes.lookup(target, correlation)
    }

    /// Reserve an immediate pump-owned mutation before applying its effect.
    /// Completion is published separately so a duplicate can never observe a
    /// terminal outcome before the in-memory effect is committed.
    pub fn begin_immediate(&self, key: OutcomeKey) -> Option<WireResponse> {
        self.outcomes.register(key)
    }

    pub fn complete_immediate(&self, key: OutcomeKey, response: WireResponse) {
        self.outcomes
            .set_terminal(key, RequestOutcome::Completed, response);
    }

    pub fn try_observe(&self) -> Result<(), AdmissionError> {
        self.submit_maintenance(Operation::Observe)
    }

    pub fn try_hotplug(&self) -> Result<(), AdmissionError> {
        self.submit_maintenance(Operation::Hotplug)
    }

    pub fn begin_suspend(&self) -> Result<(), AdmissionError> {
        self.begin_lifecycle(LifecycleOperation::Suspend, None)
    }

    pub fn begin_resume(&self) -> Result<(), AdmissionError> {
        self.begin_lifecycle(LifecycleOperation::Resume, None)
    }

    pub fn drain_events(&self) -> Vec<ExecutorEvent> {
        self.events.lock().unwrap().drain()
    }

    /// Close admission, finish all work already admitted ahead of the
    /// shutdown barrier, release hardware through Backend, then join the one
    /// owner thread. No started transaction is cancelled.
    pub fn shutdown(&self) {
        let Some(owner) = self.owner.lock().unwrap().take() else {
            return;
        };
        if !matches!(
            self.mode(),
            AdmissionMode::Stopped | AdmissionMode::ShuttingDown
        ) {
            // Do not wait on an acknowledgement here. Closing the sender
            // after enqueueing is sufficient: a live owner drains the FIFO
            // and observes Shutdown, while a failed owner observes receiver
            // disconnect and runs its release path.
            let _ = self.begin_lifecycle(LifecycleOperation::Shutdown, None);
        }
        // If admission could not enqueue the final barrier, closing the sole
        // sender makes the owner leave recv() and its disconnect path releases
        // direct control. A successful barrier also gets a clean close before
        // joining.
        self.sender.lock().unwrap().take();
        let _ = owner.join();
    }

    fn admit_client(&self, job: Job) -> Result<(), AdmissionError> {
        let mut admission = self.admission.lock().unwrap();
        admission.admit_client()?;
        let sender = self.sender.lock().unwrap();
        let Some(sender) = sender.as_ref() else {
            admission.release_queue_slot();
            return Err(AdmissionError::Stopped);
        };
        match sender.try_send(job) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                admission.release_queue_slot();
                Err(AdmissionError::Full)
            }
            Err(TrySendError::Disconnected(_)) => {
                admission.release_queue_slot();
                admission.mode = AdmissionMode::Stopped;
                Err(AdmissionError::Stopped)
            }
        }
    }

    fn submit_maintenance(&self, operation: Operation) -> Result<(), AdmissionError> {
        let mut admission = self.admission.lock().unwrap();
        admission.admit_maintenance()?;
        let sender = self.sender.lock().unwrap();
        let Some(sender) = sender.as_ref() else {
            admission.release_queue_slot();
            return Err(AdmissionError::Stopped);
        };
        match sender.try_send(Job { operation }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                admission.release_queue_slot();
                Err(AdmissionError::Full)
            }
            Err(TrySendError::Disconnected(_)) => {
                admission.release_queue_slot();
                admission.mode = AdmissionMode::Stopped;
                Err(AdmissionError::Stopped)
            }
        }
    }

    fn begin_lifecycle(
        &self,
        operation: LifecycleOperation,
        ack: Option<SyncSender<()>>,
    ) -> Result<(), AdmissionError> {
        let mut admission = self.admission.lock().unwrap();
        let previous_mode = admission.mode;
        admission.begin_lifecycle(operation)?;
        let sender = self.sender.lock().unwrap();
        let Some(sender) = sender.as_ref() else {
            admission.release_queue_slot();
            admission.mode = previous_mode;
            return Err(AdmissionError::Stopped);
        };
        match sender.try_send(Job {
            operation: Operation::Lifecycle { operation, ack },
        }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                admission.release_queue_slot();
                admission.mode = previous_mode;
                Err(AdmissionError::Full)
            }
            Err(TrySendError::Disconnected(_)) => {
                admission.release_queue_slot();
                admission.mode = AdmissionMode::Stopped;
                Err(AdmissionError::Stopped)
            }
        }
    }
}

impl Drop for BackendExecutor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_owner(
    backend: &mut Backend,
    receiver: Receiver<Job>,
    admission: Arc<Mutex<Admission>>,
    events: Arc<Mutex<EventQueue>>,
    outcomes: Arc<OutcomeStore>,
) {
    while let Ok(job) = receiver.recv() {
        admission.lock().unwrap().release_queue_slot();
        match job.operation {
            Operation::Command {
                key,
                command,
                effects,
                peer,
                deadline,
                reply,
            } => {
                let request_id = key.request_id;
                let now = Instant::now();
                let peer_alive = peer.is_alive();
                if !peer_alive {
                    let mut response = failure(
                        Some(request_id),
                        ErrorCode::Rejected,
                        "request disconnected before start",
                    );
                    response.outcome = RequestOutcome::Disconnected;
                    outcomes.set_terminal(key, RequestOutcome::Disconnected, response);
                    continue;
                }
                if now >= deadline {
                    let mut response = failure(
                        Some(request_id),
                        ErrorCode::Timeout,
                        "request expired before start",
                    );
                    response.outcome = RequestOutcome::Expired;
                    outcomes.set_terminal(key, RequestOutcome::Expired, response.clone());
                    let _ = reply.send(response);
                    continue;
                }
                if !can_start(now, deadline, peer_alive) {
                    continue;
                }
                outcomes.set_started(key);
                // The owner never waits for a client to acknowledge progress;
                // the bounded reply channel can hold started and completed.
                let _ = reply.send(WireResponse::progress(request_id, RequestOutcome::Started));
                let response = execute(backend, request_id, *command);
                if response.ok {
                    let mut event_queue = events.lock().unwrap();
                    event_queue.publish_effects(&effects);
                }
                outcomes.set_terminal(key, RequestOutcome::Completed, response.clone());
                // A response receiver may be gone. Effects were committed
                // above and remain independent of this socket write.
                let _ = reply.send(response);
            }
            Operation::Observe => backend.observe(),
            Operation::Hotplug => backend.restore_hotplugged_lighting(),
            Operation::Lifecycle { operation, ack } => {
                match operation {
                    LifecycleOperation::Suspend => {
                        backend.shutdown();
                        admission
                            .lock()
                            .unwrap()
                            .barrier_finished(LifecycleOperation::Suspend);
                        events
                            .lock()
                            .unwrap()
                            .publish_lifecycle(ExecutorEvent::Suspended);
                    }
                    LifecycleOperation::Resume => {
                        let changed = backend.restore_volatile();
                        admission
                            .lock()
                            .unwrap()
                            .barrier_finished(LifecycleOperation::Resume);
                        events
                            .lock()
                            .unwrap()
                            .publish_lifecycle(ExecutorEvent::Resumed(changed));
                    }
                    LifecycleOperation::Shutdown => {
                        backend.shutdown();
                        admission
                            .lock()
                            .unwrap()
                            .barrier_finished(LifecycleOperation::Shutdown);
                    }
                }
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
                if matches!(operation, LifecycleOperation::Shutdown) {
                    return;
                }
            }
        }
    }
    // The sender can disappear if shutdown admission itself fails. Even in
    // that path the sole backend owner must release direct EC control.
    backend.shutdown();
    admission.lock().unwrap().mode = AdmissionMode::Stopped;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_reserves_slots_for_lifecycle() {
        let mut admission = Admission::new();
        for _ in 0..(BACKEND_QUEUE_CAPACITY - LIFECYCLE_RESERVE) {
            admission.admit_client().unwrap();
        }
        assert_eq!(admission.admit_client(), Err(AdmissionError::Full));
        admission
            .begin_lifecycle(LifecycleOperation::Suspend)
            .unwrap();
        assert_eq!(admission.mode(), AdmissionMode::BarrierPending);
    }

    #[test]
    fn admission_rejects_new_work_after_barrier_and_resume_reopens_it() {
        let mut admission = Admission::new();
        admission
            .begin_lifecycle(LifecycleOperation::Suspend)
            .unwrap();
        assert_eq!(
            admission.admit_client(),
            Err(AdmissionError::BarrierPending)
        );
        admission.barrier_finished(LifecycleOperation::Suspend);
        assert_eq!(admission.admit_client(), Err(AdmissionError::Suspended));
        admission
            .begin_lifecycle(LifecycleOperation::Resume)
            .unwrap();
        admission.barrier_finished(LifecycleOperation::Resume);
        assert_eq!(admission.mode(), AdmissionMode::Active);
        admission.admit_client().unwrap();
    }

    #[test]
    fn failed_lifecycle_enqueue_restores_the_exact_previous_mode() {
        let mut admission = Admission::new();
        admission.queued = BACKEND_QUEUE_CAPACITY;
        assert_eq!(
            admission.begin_lifecycle(LifecycleOperation::Suspend),
            Err(AdmissionError::Full)
        );
        assert_eq!(admission.mode(), AdmissionMode::Active);

        admission.mode = AdmissionMode::Suspended;
        assert_eq!(
            admission.begin_lifecycle(LifecycleOperation::Resume),
            Err(AdmissionError::Full)
        );
        assert_eq!(admission.mode(), AdmissionMode::Suspended);
    }

    #[test]
    fn queued_admission_is_fifo_by_sync_channel_order() {
        let (sender, receiver) = mpsc::sync_channel(4);
        sender.send(1).unwrap();
        sender.send(2).unwrap();
        sender.send(3).unwrap();
        assert_eq!(receiver.recv().unwrap(), 1);
        assert_eq!(receiver.recv().unwrap(), 2);
        assert_eq!(receiver.recv().unwrap(), 3);
    }

    #[test]
    fn peer_liveness_detects_disconnect_without_touching_hardware() {
        let (peer, stream) = UnixStream::pair().unwrap();
        let liveness = PeerLiveness::from_stream(&stream).unwrap();
        assert!(liveness.is_alive());
        drop(peer);
        assert!(!liveness.is_alive());
    }

    #[test]
    fn expired_or_disconnected_jobs_are_rejected_before_start() {
        let now = Instant::now();
        assert!(!can_start(now, now - Duration::from_millis(1), true));
        assert!(!can_start(now, now + Duration::from_secs(1), false));
        assert!(can_start(now, now + Duration::from_secs(1), true));

        let (peer, stream) = UnixStream::pair().unwrap();
        let live = PeerLiveness::from_stream(&stream).unwrap();
        assert!(live.is_alive());
        drop(peer);
        assert!(!live.is_alive());
    }

    #[test]
    fn effect_queue_coalesces_state_invalidations_but_keeps_barriers() {
        let mut queue = EventQueue::default();
        queue.publish_effects(&[Effect::StateChanged]);
        queue.publish_effects(&[Effect::StateChanged]);
        queue.publish_lifecycle(ExecutorEvent::Suspended);
        assert_eq!(
            queue.drain(),
            vec![ExecutorEvent::StateChanged, ExecutorEvent::Suspended]
        );
    }

    #[test]
    fn lifecycle_notifications_are_bounded() {
        let mut queue = EventQueue::default();
        for _ in 0..(MAX_LIFECYCLE_EVENTS * 3) {
            queue.publish_lifecycle(ExecutorEvent::Suspended);
        }
        assert!(queue.lifecycle.len() <= MAX_LIFECYCLE_EVENTS);
    }

    #[test]
    fn outcomes_are_idempotent_correlated_and_bounded() {
        let store = OutcomeStore::default();
        let client = ClientId::new(1).unwrap();
        let first = RequestId::new(1).unwrap();
        let first_key = OutcomeKey::new(client, first);
        let reconnect = RequestId::new(99).unwrap();
        assert!(store.register(first_key).is_none());
        assert_eq!(
            store.register(first_key).unwrap().outcome,
            RequestOutcome::Queued
        );
        store.set_started(first_key);
        assert_eq!(
            store.lookup(first_key, reconnect).unwrap().outcome,
            RequestOutcome::Started
        );
        store.set_terminal(
            first_key,
            RequestOutcome::Completed,
            WireResponse::success(first),
        );
        let completed = store.lookup(first_key, reconnect).unwrap();
        assert_eq!(completed.request_id, Some(reconnect));
        assert_eq!(completed.outcome_client_id, Some(client));
        assert_eq!(completed.outcome_request_id, Some(first));
        assert_eq!(completed.outcome, RequestOutcome::Completed);

        for value in 2..=(MAX_RETAINED_OUTCOMES as u64 + 1) {
            let request_id = RequestId::new(value).unwrap();
            let key = OutcomeKey::new(client, request_id);
            store.register(key);
            store.set_terminal(
                key,
                RequestOutcome::Completed,
                WireResponse::success(request_id),
            );
        }
        assert!(store.lookup(first_key, reconnect).is_none());
        assert!(
            store
                .lookup(
                    OutcomeKey::new(
                        client,
                        RequestId::new(MAX_RETAINED_OUTCOMES as u64 + 1).unwrap(),
                    ),
                    reconnect,
                )
                .is_some()
        );
    }

    #[test]
    fn immediate_outcome_stays_queued_until_the_effect_boundary_completes() {
        let store = OutcomeStore::default();
        let request_id = RequestId::new(41).unwrap();
        let key = OutcomeKey::new(ClientId::new(1).unwrap(), request_id);
        assert!(store.register(key).is_none());
        assert_eq!(
            store.lookup(key, request_id).unwrap().outcome,
            RequestOutcome::Queued
        );
        store.set_terminal(
            key,
            RequestOutcome::Completed,
            WireResponse::success(request_id),
        );
        assert_eq!(
            store.lookup(key, request_id).unwrap().outcome,
            RequestOutcome::Completed
        );
    }

    #[test]
    fn terminal_snapshot_preserves_expired_outcome() {
        let store = OutcomeStore::default();
        let request_id = RequestId::new(42).unwrap();
        let key = OutcomeKey::new(ClientId::new(1).unwrap(), request_id);
        assert!(store.register(key).is_none());
        let mut response = failure(
            Some(request_id),
            ErrorCode::Timeout,
            "request expired before start",
        );
        response.outcome = RequestOutcome::Expired;
        store.set_terminal(key, RequestOutcome::Expired, response);
        let snapshot = store.lookup(key, request_id).unwrap();
        assert_eq!(snapshot.outcome, RequestOutcome::Expired);
        assert!(snapshot.error.is_some());
    }

    #[test]
    fn equal_request_ids_from_different_clients_do_not_alias() {
        let store = OutcomeStore::default();
        let request_id = RequestId::new(1).unwrap();
        let first = OutcomeKey::new(ClientId::new(1).unwrap(), request_id);
        let second = OutcomeKey::new(ClientId::new(2).unwrap(), request_id);
        assert!(store.register(first).is_none());
        assert!(store.register(second).is_none());
        store.set_terminal(
            first,
            RequestOutcome::Completed,
            WireResponse::success(request_id),
        );
        assert_eq!(
            store.lookup(first, request_id).unwrap().outcome,
            RequestOutcome::Completed
        );
        assert_eq!(
            store.lookup(second, request_id).unwrap().outcome,
            RequestOutcome::Queued
        );
    }
}
