use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use z13helper_core::curve::Curve;
use z13helper_core::protocol::{FanHysteresis, ProbeReply};

use crate::ec::{EcMailbox, LinuxPortIo, PortIo};
use crate::service::{Controller, DIRECT_TICK_INTERVAL};

const COMMAND_CAPACITY: usize = 8;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
struct DirectPolicy {
    hysteresis: FanHysteresis,
    temperature_average_seconds: u8,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DirectSnapshot {
    pub enabled: bool,
    pub held: bool,
    pub last_safe_duty: [u8; 2],
    pub temperature_millic: Option<i32>,
    pub last_error: Option<String>,
    pub release_failure: Option<String>,
}

struct Fenced<T> {
    fence: u64,
    result: Result<T, String>,
}

type Reply<T> = SyncSender<Fenced<T>>;

enum Command {
    SetPolicy {
        fence: u64,
        policy: DirectPolicy,
        reply: Reply<()>,
    },
    InstallPrime {
        fence: u64,
        curves: [Curve; 2],
        reply: Reply<DirectSnapshot>,
    },
    Hold {
        fence: u64,
        reply: Reply<DirectSnapshot>,
    },
    Resume {
        fence: u64,
        reply: Reply<DirectSnapshot>,
    },
    Release {
        fence: u64,
        reply: Reply<DirectSnapshot>,
    },
    Snapshot {
        fence: u64,
        reply: Reply<DirectSnapshot>,
    },
}

pub struct DirectRuntime {
    commands: SyncSender<Command>,
    shutdown_requested: Arc<AtomicBool>,
    shutdown_result: Receiver<Result<(), String>>,
    next_fence: u64,
    owner: Option<JoinHandle<()>>,
    shutdown: bool,
}

impl DirectRuntime {
    pub fn start() -> Result<(Self, ProbeReply), String> {
        spawn_owner(
            || {
                let io = LinuxPortIo::acquire().map_err(|error| error.to_string())?;
                Ok(Controller::new(EcMailbox::new(io)))
            },
            || crate::sensors::read_temperature_millic().map_err(|error| error.to_string()),
            true,
        )
    }

    pub fn set_policy(
        &mut self,
        hysteresis: FanHysteresis,
        temperature_average_seconds: u8,
    ) -> Result<(), String> {
        let fence = self.fence();
        let (reply, receiver) = mpsc::sync_channel(1);
        self.call(
            Command::SetPolicy {
                fence,
                policy: DirectPolicy {
                    hysteresis,
                    temperature_average_seconds,
                },
                reply,
            },
            receiver,
        )
    }

    pub fn install_and_prime(&mut self, curves: [Curve; 2]) -> Result<DirectSnapshot, String> {
        let fence = self.fence();
        let (reply, receiver) = mpsc::sync_channel(1);
        self.call(
            Command::InstallPrime {
                fence,
                curves,
                reply,
            },
            receiver,
        )
    }

    /// Fence direct ticks while firmware owns the fan endpoints. This does
    /// not release EC control, so the owner retains its last known-safe duty
    /// if the firmware write must be rolled back.
    pub fn hold_for_firmware(&mut self) -> Result<DirectSnapshot, String> {
        let fence = self.fence();
        let (reply, receiver) = mpsc::sync_channel(1);
        self.call(Command::Hold { fence, reply }, receiver)
    }

    /// Resume direct control after a firmware curve write failed. The fenced
    /// transition makes the next tick observe the retained direct duty.
    pub fn resume_after_firmware_failure(&mut self) -> Result<DirectSnapshot, String> {
        let fence = self.fence();
        let (reply, receiver) = mpsc::sync_channel(1);
        self.call(Command::Resume { fence, reply }, receiver)
    }

    pub fn release(&mut self) -> Result<DirectSnapshot, String> {
        let fence = self.fence();
        let (reply, receiver) = mpsc::sync_channel(1);
        self.call(Command::Release { fence, reply }, receiver)
    }

    pub fn snapshot(&mut self) -> Result<DirectSnapshot, String> {
        let fence = self.fence();
        let (reply, receiver) = mpsc::sync_channel(1);
        self.call(Command::Snapshot { fence, reply }, receiver)
    }

    /// Stop the owner thread after releasing direct EC control. The command is
    /// idempotent because suspend uses release while final process teardown
    /// uses shutdown.
    pub fn shutdown(&mut self) -> Result<(), String> {
        if self.shutdown {
            return Ok(());
        }
        self.shutdown_requested.store(true, Ordering::Release);
        self.shutdown_with_timeout(COMMAND_TIMEOUT)
    }

    fn shutdown_with_timeout(&mut self, timeout: Duration) -> Result<(), String> {
        if self.shutdown {
            return Ok(());
        }
        let result = match self.shutdown_result.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Keep the JoinHandle. The owner still owns the EC and will
                // process the out-of-band shutdown request after its bounded
                // current mailbox/sensor operation completes.
                return Err("direct runtime shutdown timed out; owner retained for release".into());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return self.join_owner().map(|_| ()).and_then(|_| {
                    Err("direct runtime owner stopped before releasing EC control".into())
                });
            }
        };
        self.join_owner()?;
        self.shutdown = true;
        result
    }

    fn join_owner(&mut self) -> Result<(), String> {
        let Some(owner) = self.owner.take() else {
            return Ok(());
        };
        owner
            .join()
            .map_err(|_| "direct runtime owner thread panicked".to_owned())
    }

    fn fence(&mut self) -> u64 {
        let fence = self.next_fence;
        self.next_fence = self.next_fence.wrapping_add(1).max(1);
        fence
    }

    fn call<T>(&self, command: Command, receiver: Receiver<Fenced<T>>) -> Result<T, String> {
        if self.shutdown || self.shutdown_requested.load(Ordering::Acquire) {
            return Err("direct runtime is shutting down".into());
        }
        let expected = match &command {
            Command::SetPolicy { fence, .. }
            | Command::InstallPrime { fence, .. }
            | Command::Hold { fence, .. }
            | Command::Resume { fence, .. }
            | Command::Release { fence, .. }
            | Command::Snapshot { fence, .. } => *fence,
        };
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let mut command = command;
        loop {
            match self.commands.try_send(command) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    command = returned;
                    if Instant::now() >= deadline {
                        return Err("direct runtime command queue is full".into());
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err("direct runtime owner thread stopped".into());
                }
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let response = receiver
            .recv_timeout(remaining)
            .map_err(|_| "direct runtime command timed out".to_owned())?;
        if response.fence != expected {
            return Err("direct runtime fence mismatch".into());
        }
        response.result
    }
}

impl Drop for DirectRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown();
        // A timeout only bounds the caller-facing shutdown operation. Do not
        // detach a live owner during final teardown: the owner must finish
        // the release it owns before its JoinHandle is dropped.
        if self.owner.is_some() {
            let _ = self.join_owner();
        }
    }
}

struct Owner<P, R> {
    controller: Controller<P>,
    temperature: R,
    policy: DirectPolicy,
    snapshot: DirectSnapshot,
    held: bool,
    release_pending: bool,
    released: bool,
    next_tick: Instant,
}

impl<P, R> Owner<P, R>
where
    P: PortIo,
    R: FnMut() -> Result<i32, String>,
{
    fn new(controller: Controller<P>, temperature: R) -> Self {
        Self {
            controller,
            temperature,
            policy: DirectPolicy {
                hysteresis: FanHysteresis::default(),
                temperature_average_seconds:
                    z13helper_core::profile::default_fan_temperature_average_seconds(),
            },
            snapshot: DirectSnapshot::default(),
            held: false,
            release_pending: false,
            released: false,
            next_tick: Instant::now() + DIRECT_TICK_INTERVAL,
        }
    }

    fn set_policy(&mut self, policy: DirectPolicy) {
        self.policy = policy;
    }

    fn install_and_prime(&mut self, curves: [Curve; 2]) -> Result<DirectSnapshot, String> {
        if let Err(error) = self.controller.enable(
            curves,
            self.policy.hysteresis,
            self.policy.temperature_average_seconds,
        ) {
            return Err(self.fail_and_release(error));
        }
        self.released = false;
        self.held = false;
        self.release_pending = false;
        let temperature = match (self.temperature)() {
            Ok(temperature) => temperature,
            Err(error) => {
                let failure = self.controller.sensor_failed(error);
                return Err(self.fail_and_release(failure));
            }
        };
        self.snapshot.temperature_millic = Some(temperature);
        if let Err(error) = self.controller.prime(Instant::now(), temperature) {
            return Err(self.fail_and_release(error));
        }
        self.snapshot.enabled = true;
        self.snapshot.held = false;
        self.snapshot.last_safe_duty = self.controller.last_duty();
        self.snapshot.last_error = None;
        self.snapshot.release_failure = None;
        Ok(self.snapshot.clone())
    }

    fn release(&mut self) -> Result<DirectSnapshot, String> {
        if self.released && !self.release_pending {
            return Ok(self.snapshot.clone());
        }
        match self.controller.release() {
            Ok(()) => {
                self.held = false;
                self.released = true;
                self.release_pending = false;
                self.snapshot.enabled = false;
                self.snapshot.held = false;
                self.snapshot.release_failure = None;
                Ok(self.snapshot.clone())
            }
            Err(error) => {
                self.release_pending = true;
                self.snapshot.release_failure = Some(error.clone());
                Err(error)
            }
        }
    }

    fn tick(&mut self, now: Instant) {
        if self.release_pending {
            let _ = self.release();
            return;
        }
        if self.held {
            return;
        }
        if !self.controller.direct_enabled() {
            return;
        }
        let temperature = match (self.temperature)() {
            Ok(temperature) => temperature,
            Err(error) => {
                let error = self.controller.sensor_failed(error);
                self.snapshot.last_error = Some(error);
                self.sync_release_failure();
                return;
            }
        };
        self.snapshot.temperature_millic = Some(temperature);
        if let Err(error) = self.controller.tick(now, temperature) {
            self.snapshot.last_error = Some(error);
            self.sync_release_failure();
            return;
        }
        self.snapshot.last_safe_duty = self.controller.last_duty();
        self.snapshot.enabled = true;
        self.snapshot.last_error = None;
    }

    fn sync_release_failure(&mut self) {
        if let Some(error) = self.controller.take_release_failure() {
            self.release_pending = true;
            self.snapshot.release_failure = Some(error);
        }
        if !self.controller.direct_enabled() && !self.release_pending {
            self.released = true;
            self.snapshot.enabled = false;
        }
    }

    fn fail_and_release(&mut self, error: String) -> String {
        let release = self.release();
        match release {
            Ok(_) => error,
            Err(release) => format!("{error}; direct EC release failed: {release}"),
        }
    }

    fn snapshot(&self) -> DirectSnapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot.held = self.held;
        snapshot
    }

    fn hold(&mut self) -> DirectSnapshot {
        if self.controller.direct_enabled() {
            self.held = true;
            self.snapshot.held = true;
        }
        self.snapshot()
    }

    fn resume(&mut self) -> Result<DirectSnapshot, String> {
        if self.release_pending {
            return Err("direct EC release is pending; cannot resume direct control".into());
        }
        self.held = false;
        self.snapshot.held = false;
        self.snapshot.enabled = self.controller.direct_enabled();
        Ok(self.snapshot())
    }

    fn command(&mut self, command: Command) -> bool {
        match command {
            Command::SetPolicy {
                fence,
                policy,
                reply,
            } => {
                self.set_policy(policy);
                let _ = reply.send(Fenced {
                    fence,
                    result: Ok(()),
                });
                false
            }
            Command::InstallPrime {
                fence,
                curves,
                reply,
            } => {
                let result = self.install_and_prime(curves);
                let _ = reply.send(Fenced { fence, result });
                false
            }
            Command::Hold { fence, reply } => {
                let _ = reply.send(Fenced {
                    fence,
                    result: Ok(self.hold()),
                });
                false
            }
            Command::Resume { fence, reply } => {
                let _ = reply.send(Fenced {
                    fence,
                    result: self.resume(),
                });
                false
            }
            Command::Release { fence, reply } => {
                let result = self.release();
                let _ = reply.send(Fenced { fence, result });
                false
            }
            Command::Snapshot { fence, reply } => {
                let _ = reply.send(Fenced {
                    fence,
                    result: Ok(self.snapshot()),
                });
                false
            }
        }
    }

    fn run(
        mut self,
        commands: Receiver<Command>,
        shutdown_requested: Arc<AtomicBool>,
        shutdown_result: SyncSender<Result<(), String>>,
    ) {
        loop {
            let now = Instant::now();
            if shutdown_requested.load(Ordering::Acquire) {
                let result = self.release().map(|_| ());
                let _ = shutdown_result.send(result);
                return;
            }
            if now >= self.next_tick {
                self.tick(now);
                self.next_tick += DIRECT_TICK_INTERVAL;
                if self.next_tick <= now {
                    self.next_tick = now + DIRECT_TICK_INTERVAL;
                }
                continue;
            }
            match commands.recv_timeout(self.next_tick - now) {
                Ok(command) => {
                    if self.command(command) {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let result = self.release().map(|_| ());
                    let _ = shutdown_result.send(result);
                    return;
                }
            }
        }
    }
}

fn spawn_owner<P, R, F>(
    factory: F,
    temperature: R,
    startup_probe: bool,
) -> Result<(DirectRuntime, ProbeReply), String>
where
    P: PortIo + Send + 'static,
    R: FnMut() -> Result<i32, String> + Send + 'static,
    F: FnOnce() -> Result<Controller<P>, String> + Send + 'static,
{
    let (commands, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let (shutdown_result, shutdown_receiver) = mpsc::sync_channel(1);
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    let owner_shutdown_requested = Arc::clone(&shutdown_requested);
    let owner = thread::Builder::new()
        .name("z13helper-direct-fan".into())
        .spawn(move || {
            let controller = match factory() {
                Ok(controller) => controller,
                Err(error) => {
                    let _ = ready_sender.send(Err(error));
                    return;
                }
            };
            let mut owner = Owner::new(controller, temperature);
            let probe = if startup_probe {
                match owner.controller.startup_release_and_probe() {
                    Ok(probe) => {
                        owner.released = true;
                        Ok(probe)
                    }
                    Err(error) => Err(error),
                }
            } else {
                Ok(ProbeReply {
                    model: "GZ302EA".into(),
                    ec_version: 0,
                    fan_count: 2,
                })
            };
            let probe = match probe {
                Ok(probe) => probe,
                Err(error) => {
                    let _ = ready_sender.send(Err(error));
                    return;
                }
            };
            let _ = ready_sender.send(Ok(probe));
            owner.run(receiver, owner_shutdown_requested, shutdown_result);
        })
        .map_err(|error| error.to_string())?;

    let probe = match ready_receiver.recv_timeout(STARTUP_TIMEOUT) {
        Ok(Ok(probe)) => probe,
        Ok(Err(error)) => {
            shutdown_requested.store(true, Ordering::Release);
            drop(commands);
            let _ = owner.join();
            return Err(error);
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The factory may have acquired raw EC ownership before the
            // startup handshake stalled. Never detach that late owner: ask
            // it to release, disconnect its command queue, and join it before
            // reporting startup failure.
            shutdown_requested.store(true, Ordering::Release);
            drop(commands);
            let _ = owner.join();
            return Err("direct runtime startup timed out".to_owned());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            shutdown_requested.store(true, Ordering::Release);
            drop(commands);
            let _ = owner.join();
            return Err("direct runtime owner stopped during startup".to_owned());
        }
    };
    Ok((
        DirectRuntime {
            commands,
            shutdown_requested,
            shutdown_result: shutdown_receiver,
            next_fence: 1,
            owner: Some(owner),
            shutdown: false,
        },
        probe,
    ))
}

impl DirectRuntime {
    #[cfg(test)]
    pub(crate) fn fake<P, R>(controller: Controller<P>, temperature: R) -> (Self, ProbeReply)
    where
        P: PortIo + Send + 'static,
        R: FnMut() -> Result<i32, String> + Send + 'static,
    {
        spawn_owner(move || Ok(controller), temperature, false).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::{COMMAND_STATUS_PORT, DATA_PORT, PortIo, Register};
    use std::io;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct FakeIo {
        writes: Arc<Mutex<Vec<(u16, u8)>>>,
    }

    impl PortIo for FakeIo {
        fn read_u8(&mut self, _port: u16) -> io::Result<u8> {
            Ok(0)
        }

        fn write_u8(&mut self, port: u16, value: u8) -> io::Result<()> {
            self.writes.lock().unwrap().push((port, value));
            Ok(())
        }
    }

    fn curve(duty: i32) -> Curve {
        [
            [20, duty],
            [30, duty],
            [40, duty],
            [50, duty],
            [60, duty],
            [70, duty],
            [80, duty],
            [90, duty],
        ]
    }

    fn changing_curve() -> Curve {
        [
            [20, 100],
            [30, 100],
            [40, 100],
            [50, 100],
            [60, 100],
            [70, 150],
            [80, 200],
            [90, 200],
        ]
    }

    fn mode_values(writes: &[(u16, u8)]) -> Vec<u8> {
        writes
            .windows(5)
            .filter(|window| {
                window[0] == (COMMAND_STATUS_PORT, 0xff)
                    && window[1] == (COMMAND_STATUS_PORT, 0xdd)
                    && window[2] == (DATA_PORT, 0x82)
                    && window[3] == (DATA_PORT, Register::GlobalMode as u8)
            })
            .map(|window| window[4].1)
            .collect()
    }

    #[test]
    fn tick_cadence_survives_a_blocked_backend_surrogate() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let tick_times = Arc::new(Mutex::new(Vec::new()));
        let io = FakeIo::default();
        let controller = Controller::new(EcMailbox::new(io));
        let tick_counter = Arc::clone(&ticks);
        let times = Arc::clone(&tick_times);
        let (mut runtime, _) = DirectRuntime::fake(controller, move || {
            tick_counter.fetch_add(1, Ordering::SeqCst);
            times.lock().unwrap().push(Instant::now());
            Ok(60_000)
        });
        runtime.set_policy(FanHysteresis::default(), 0).unwrap();
        runtime.install_and_prime([curve(100), curve(100)]).unwrap();

        let backend_surrogate = Arc::new(Mutex::new(()));
        let guard = backend_surrogate.lock().unwrap();
        let started = Instant::now();
        thread::sleep(Duration::from_millis(700));
        drop(guard);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(ticks.load(Ordering::SeqCst) >= 2);
        let times = tick_times.lock().unwrap();
        for pair in times.windows(2) {
            assert!(pair[1].duration_since(pair[0]) < Duration::from_millis(500));
        }
    }

    #[test]
    fn install_prime_and_release_are_fenced_in_owner_order() {
        let io = FakeIo::default();
        let writes = Arc::clone(&io.writes);
        let controller = Controller::new(EcMailbox::new(io));
        let (mut runtime, _) = DirectRuntime::fake(controller, || Ok(60_000));
        runtime.set_policy(FanHysteresis::default(), 0).unwrap();
        runtime.install_and_prime([curve(100), curve(120)]).unwrap();
        runtime.release().unwrap();
        let modes = mode_values(&writes.lock().unwrap());
        assert_eq!(modes, [true as u8, false as u8]);
        let snapshot = runtime.snapshot().unwrap();
        assert!(!snapshot.enabled);
        assert_eq!(snapshot.last_safe_duty, [100, 120]);
    }

    #[test]
    fn firmware_hold_fences_ticks_and_resume_keeps_last_safe_duty() {
        let temperature = Arc::new(AtomicUsize::new(60_000));
        let current_temperature = Arc::clone(&temperature);
        let io = FakeIo::default();
        let writes = Arc::clone(&io.writes);
        let controller = Controller::new(EcMailbox::new(io));
        let (mut runtime, _) = DirectRuntime::fake(controller, move || {
            Ok(current_temperature.load(Ordering::SeqCst) as i32)
        });
        runtime.set_policy(FanHysteresis::default(), 0).unwrap();
        runtime
            .install_and_prime([changing_curve(), changing_curve()])
            .unwrap();
        let before_hold = runtime.snapshot().unwrap();
        assert_eq!(before_hold.last_safe_duty, [100, 100]);

        let held = runtime.hold_for_firmware().unwrap();
        assert!(held.held);
        temperature.store(80_000, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(500));
        let during_hold = runtime.snapshot().unwrap();
        assert!(during_hold.held);
        assert_eq!(during_hold.last_safe_duty, [100, 100]);
        assert_eq!(mode_values(&writes.lock().unwrap()), [true as u8]);

        let resumed = runtime.resume_after_firmware_failure().unwrap();
        assert!(!resumed.held);
        thread::sleep(Duration::from_millis(500));
        let after_resume = runtime.snapshot().unwrap();
        assert!(after_resume.last_safe_duty[0] > before_hold.last_safe_duty[0]);
        assert_eq!(mode_values(&writes.lock().unwrap()), [true as u8]);
    }

    #[test]
    fn shutdown_is_idempotent_and_snapshot_errors_after_owner_exit() {
        let controller = Controller::new(EcMailbox::new(FakeIo::default()));
        let (mut runtime, _) = DirectRuntime::fake(controller, || Ok(60_000));
        runtime.shutdown().unwrap();
        runtime.shutdown().unwrap();
        assert!(runtime.snapshot().is_err());
    }

    #[test]
    fn shutdown_timeout_retains_owner_until_release_completes() {
        let block = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        let block_temperature = Arc::clone(&block);
        let entered_temperature = Arc::clone(&entered);
        let controller = Controller::new(EcMailbox::new(FakeIo::default()));
        let (mut runtime, _) = DirectRuntime::fake(controller, move || {
            if block_temperature.load(Ordering::SeqCst) {
                entered_temperature.store(true, Ordering::SeqCst);
                while block_temperature.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            Ok(60_000)
        });
        runtime.set_policy(FanHysteresis::default(), 0).unwrap();
        runtime.install_and_prime([curve(100), curve(100)]).unwrap();
        block.store(true, Ordering::SeqCst);
        while !entered.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }

        runtime.shutdown_requested.store(true, Ordering::Release);
        let timeout = runtime.shutdown_with_timeout(Duration::from_millis(10));
        assert!(timeout.is_err());
        assert!(runtime.owner.is_some());

        block.store(false, Ordering::SeqCst);
        runtime
            .shutdown_with_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(runtime.owner.is_none());
        runtime.shutdown().unwrap();
    }

    #[test]
    fn snapshot_propagates_sensor_errors_without_hardware() {
        let fail = Arc::new(AtomicBool::new(false));
        let controller = Controller::new(EcMailbox::new(FakeIo::default()));
        let sensor_fail = Arc::clone(&fail);
        let (mut runtime, _) = DirectRuntime::fake(controller, move || {
            if sensor_fail.load(Ordering::SeqCst) {
                Err("fake sensor failed".into())
            } else {
                Ok(60_000)
            }
        });
        runtime.set_policy(FanHysteresis::default(), 0).unwrap();
        runtime.install_and_prime([curve(100), curve(100)]).unwrap();
        fail.store(true, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(350));
        let snapshot = runtime.snapshot().unwrap();
        assert!(
            snapshot
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("fake sensor failed"))
        );
        assert!(!snapshot.enabled);
    }
}
