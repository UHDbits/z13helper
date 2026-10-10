use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use z13helper_core::curve::Curve;
use z13helper_core::protocol::{FanHysteresis, ProbeReply};

use crate::ec::{EcMailbox, LinuxPortIo, PortIo};
use crate::service::{Controller, DIRECT_TICK_INTERVAL, MODEL};

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

type Reply<T> = SyncSender<Result<T, String>>;

enum Command {
    SetPolicy(DirectPolicy, Reply<()>),
    InstallPrime([Curve; 2], Reply<DirectSnapshot>),
    Hold(Reply<DirectSnapshot>),
    Resume(Reply<DirectSnapshot>),
    Release(Reply<DirectSnapshot>),
    Snapshot(Reply<DirectSnapshot>),
}

pub struct DirectRuntime {
    commands: SyncSender<Command>,
    shutdown_requested: Arc<AtomicBool>,
    shutdown_result: Receiver<Result<(), String>>,
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
        let policy = DirectPolicy {
            hysteresis,
            temperature_average_seconds,
        };
        self.call(|reply| Command::SetPolicy(policy, reply))
    }

    pub fn install_and_prime(&mut self, curves: [Curve; 2]) -> Result<DirectSnapshot, String> {
        self.call(|reply| Command::InstallPrime(curves, reply))
    }

    /// Fence direct ticks while firmware owns the fan endpoints. This does
    /// not release EC control, so the owner retains its last known-safe duty
    /// if the firmware write must be rolled back.
    pub fn hold_for_firmware(&mut self) -> Result<DirectSnapshot, String> {
        self.call(Command::Hold).inspect_err(|_| {
            // A timed-out Hold stays queued and may still run. Queue a
            // Resume behind it so it cannot stop ticks for good.
            let _ = self.resume_after_firmware_failure();
        })
    }

    /// Resume direct control after a firmware curve write failed. The fenced
    /// transition makes the next tick observe the retained direct duty.
    pub fn resume_after_firmware_failure(&mut self) -> Result<DirectSnapshot, String> {
        self.call(Command::Resume)
    }

    pub fn release(&mut self) -> Result<DirectSnapshot, String> {
        self.call(Command::Release)
    }

    pub fn snapshot(&mut self) -> Result<DirectSnapshot, String> {
        self.call(Command::Snapshot)
    }

    /// Stop the owner thread after releasing direct EC control. The command is
    /// idempotent because suspend uses release while final process teardown
    /// uses shutdown.
    pub fn shutdown(&mut self) -> Result<(), String> {
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
                self.join_owner()?;
                return Err("direct runtime owner stopped before releasing EC control".into());
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

    fn call<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<T, String> {
        if self.shutdown || self.shutdown_requested.load(Ordering::Acquire) {
            return Err("direct runtime is shutting down".into());
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        let mut command = command(reply);
        let deadline = Instant::now() + COMMAND_TIMEOUT;
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
        receiver
            .recv_timeout(remaining)
            .map_err(|_| "direct runtime command timed out".to_owned())?
    }
}

impl Drop for DirectRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown();
        // A timeout only bounds the caller-facing shutdown operation. Do not
        // detach a live owner during final teardown: the owner must finish
        // the release it owns before its JoinHandle is dropped.
        let _ = self.join_owner();
    }
}

struct Owner<P, R> {
    controller: Controller<P>,
    temperature: R,
    policy: DirectPolicy,
    /// Also the owner's state: `snapshot.held` fences direct ticks.
    snapshot: DirectSnapshot,
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
            release_pending: false,
            released: false,
            next_tick: Instant::now() + DIRECT_TICK_INTERVAL,
        }
    }

    fn install_and_prime(&mut self, curves: [Curve; 2]) -> Result<DirectSnapshot, String> {
        // A failed enable may still have reached the EC, so its release must
        // not be skipped as already done.
        self.released = false;
        if let Err(error) = self.controller.enable(
            curves,
            self.policy.hysteresis,
            self.policy.temperature_average_seconds,
        ) {
            return Err(self.fail_and_release(error));
        }
        self.snapshot.held = false;
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
        if self.snapshot.held || !self.controller.direct_enabled() {
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
        match self.release() {
            Ok(_) => error,
            Err(release) => format!("{error}; direct EC release failed: {release}"),
        }
    }

    fn hold(&mut self) -> DirectSnapshot {
        if self.controller.direct_enabled() {
            self.snapshot.held = true;
        }
        self.snapshot.clone()
    }

    fn resume(&mut self) -> Result<DirectSnapshot, String> {
        if self.release_pending {
            return Err("direct EC release is pending; cannot resume direct control".into());
        }
        self.snapshot.held = false;
        self.snapshot.enabled = self.controller.direct_enabled();
        Ok(self.snapshot.clone())
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::SetPolicy(policy, reply) => {
                self.policy = policy;
                let _ = reply.send(Ok(()));
            }
            Command::InstallPrime(curves, reply) => {
                let _ = reply.send(self.install_and_prime(curves));
            }
            Command::Hold(reply) => {
                let _ = reply.send(Ok(self.hold()));
            }
            Command::Resume(reply) => {
                let _ = reply.send(self.resume());
            }
            Command::Release(reply) => {
                let _ = reply.send(self.release());
            }
            Command::Snapshot(reply) => {
                let _ = reply.send(Ok(self.snapshot.clone()));
            }
        }
    }

    fn run(
        mut self,
        commands: Receiver<Command>,
        shutdown_requested: Arc<AtomicBool>,
        shutdown_result: SyncSender<Result<(), String>>,
    ) {
        while !shutdown_requested.load(Ordering::Acquire) {
            let now = Instant::now();
            if now >= self.next_tick {
                self.tick(now);
                self.next_tick += DIRECT_TICK_INTERVAL;
                if self.next_tick <= now {
                    self.next_tick = now + DIRECT_TICK_INTERVAL;
                }
                continue;
            }
            match commands.recv_timeout(self.next_tick - now) {
                Ok(command) => self.command(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = shutdown_result.send(self.release().map(|_| ()));
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
            let mut owner = match factory() {
                Ok(controller) => Owner::new(controller, temperature),
                Err(error) => {
                    let _ = ready_sender.send(Err(error));
                    return;
                }
            };
            let probe = if startup_probe {
                owner
                    .controller
                    .startup_release_and_probe()
                    .inspect(|_| owner.released = true)
            } else {
                Ok(ProbeReply {
                    model: MODEL.into(),
                    ec_version: 0,
                    fan_count: 2,
                })
            };
            let started = probe.is_ok();
            let _ = ready_sender.send(probe);
            if started {
                owner.run(receiver, owner_shutdown_requested, shutdown_result);
            }
        })
        .map_err(|error| error.to_string())?;

    let ready = ready_receiver
        .recv_timeout(STARTUP_TIMEOUT)
        .unwrap_or_else(|error| {
            Err(match error {
                mpsc::RecvTimeoutError::Timeout => "direct runtime startup timed out",
                mpsc::RecvTimeoutError::Disconnected => {
                    "direct runtime owner stopped during startup"
                }
            }
            .to_owned())
        });
    let probe = match ready {
        Ok(probe) => probe,
        Err(error) => {
            // The factory may have acquired raw EC ownership before startup
            // failed or stalled. Never detach that late owner: ask it to
            // release, disconnect its command queue, and join it before
            // reporting startup failure.
            shutdown_requested.store(true, Ordering::Release);
            drop(commands);
            let _ = owner.join();
            return Err(error);
        }
    };
    Ok((
        DirectRuntime {
            commands,
            shutdown_requested,
            shutdown_result: shutdown_receiver,
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
        /// Number of upcoming port writes to fail.
        fail_writes: Arc<AtomicUsize>,
    }

    impl PortIo for FakeIo {
        fn read_u8(&mut self, _port: u16) -> io::Result<u8> {
            Ok(0)
        }

        fn write_u8(&mut self, port: u16, value: u8) -> io::Result<()> {
            if self
                .fail_writes
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(io::Error::other("write failed"));
            }
            self.writes.lock().unwrap().push((port, value));
            Ok(())
        }
    }

    /// Temperature source that parks the owner inside a tick while `block`
    /// is set; `entered` reports that the owner is parked.
    fn blocking_temperature() -> (
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        impl FnMut() -> Result<i32, String> + Send + 'static,
    ) {
        let block = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        let (block_temperature, entered_temperature) = (Arc::clone(&block), Arc::clone(&entered));
        let temperature = move || {
            if block_temperature.load(Ordering::SeqCst) {
                entered_temperature.store(true, Ordering::SeqCst);
                while block_temperature.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
            }
            Ok(60_000)
        };
        (block, entered, temperature)
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
    fn failed_install_after_release_still_releases_the_ec() {
        let io = FakeIo::default();
        let writes = Arc::clone(&io.writes);
        let fail_writes = Arc::clone(&io.fail_writes);
        let controller = Controller::new(EcMailbox::new(io));
        let (mut runtime, _) = DirectRuntime::fake(controller, || Ok(60_000));
        runtime.set_policy(FanHysteresis::default(), 0).unwrap();
        runtime.install_and_prime([curve(100), curve(100)]).unwrap();
        runtime.release().unwrap();

        // Both mailbox attempts of the enable fail; it may still have reached
        // the EC, so the failure path must write automatic mode again.
        fail_writes.store(2, Ordering::SeqCst);
        assert!(runtime.install_and_prime([curve(100), curve(100)]).is_err());
        assert_eq!(
            mode_values(&writes.lock().unwrap()),
            [true as u8, false as u8, false as u8]
        );
    }

    #[test]
    fn timed_out_firmware_hold_cannot_strand_direct_ticks() {
        let (block, entered, temperature) = blocking_temperature();
        let controller = Controller::new(EcMailbox::new(FakeIo::default()));
        let (mut runtime, _) = DirectRuntime::fake(controller, temperature);
        runtime.set_policy(FanHysteresis::default(), 0).unwrap();
        runtime.install_and_prime([curve(100), curve(100)]).unwrap();
        block.store(true, Ordering::SeqCst);
        while !entered.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(1));
        }

        // The owner is busy past the command timeout, so the Hold is still
        // queued when the caller gives up and skips the firmware write.
        assert!(runtime.hold_for_firmware().is_err());
        block.store(false, Ordering::SeqCst);
        assert!(!runtime.snapshot().unwrap().held);
    }

    #[test]
    fn firmware_hold_fences_ticks_and_resume_keeps_last_safe_duty() {
        let temperature = Arc::new(AtomicUsize::new(60_000));
        let current_temperature = Arc::clone(&temperature);
        let tick_times = Arc::new(Mutex::new(Vec::new()));
        let times = Arc::clone(&tick_times);
        let io = FakeIo::default();
        let writes = Arc::clone(&io.writes);
        let controller = Controller::new(EcMailbox::new(io));
        let (mut runtime, _) = DirectRuntime::fake(controller, move || {
            times.lock().unwrap().push(Instant::now());
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
        let ticks_before_resume = tick_times.lock().unwrap().len();
        thread::sleep(Duration::from_millis(700));
        let after_resume = runtime.snapshot().unwrap();
        assert!(after_resume.last_safe_duty[0] > before_hold.last_safe_duty[0]);
        assert_eq!(mode_values(&writes.lock().unwrap()), [true as u8]);
        // Ticks keep their cadence once the hold is lifted.
        let times = tick_times.lock().unwrap();
        let resumed_ticks = &times[ticks_before_resume..];
        assert!(resumed_ticks.len() >= 2);
        for pair in resumed_ticks.windows(2) {
            assert!(pair[1].duration_since(pair[0]) < Duration::from_millis(500));
        }
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
        let (block, entered, temperature) = blocking_temperature();
        let controller = Controller::new(EcMailbox::new(FakeIo::default()));
        let (mut runtime, _) = DirectRuntime::fake(controller, temperature);
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
