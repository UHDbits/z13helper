//! Power-source detection: UPower D-Bus primary, one-shot sysfs recovery.
//!
//! Debounced transitions apply the remembered per-source profile and show the HUD.

use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use z13helper_core::debounce::{
    ConfirmationTimer, DebounceAction, PowerDebouncer, PowerObservation,
};

use crate::app::AppState;
use crate::ui::hud;

thread_local! {
    // Startup and resume both run on GTK's main thread. Keeping the coordinator
    // here lets resume invalidate pre-suspend timers without making AppState
    // own GTK-thread-only state or sending a socket call from a worker.
    static COORDINATOR: std::cell::RefCell<Option<std::rc::Rc<std::cell::RefCell<PowerDebouncer>>>> = const { std::cell::RefCell::new(None) };
}

/// Start watching power source. The D-Bus owner lives for the UI process and
/// exits when its bounded GLib delivery channel is closed. It never falls
/// back to a recurring sysfs poll.
pub fn start(state: &Rc<AppState>) {
    // Power state is a snapshot, not an event log. Keep only the newest value
    // if GTK is temporarily busy.
    let (tx, rx) = async_channel::bounded::<PowerObservation>(1);

    // Background UPower watcher with one explicit sysfs sample per outage.
    let watcher_tx = tx.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("z13helper-power".into())
        .spawn(move || {
            upower_watch(watcher_tx);
        })
    {
        tracing::error!(%error, "could not start power-source watcher");
        let _ = publish(&tx, PowerObservation::Unknown);
    }
    drop(tx);

    let debouncer = Rc::new(std::cell::RefCell::new(PowerDebouncer::new(
        state.config.borrow().power_source_debounce_ms,
    )));
    COORDINATOR.with(|coordinator| {
        *coordinator.borrow_mut() = Some(debouncer.clone());
    });
    let state = state.clone();

    glib::MainContext::default().spawn_local(async move {
        while let Ok(observation) = rx.recv().await {
            let now = monotonic_millis();
            debouncer
                .borrow_mut()
                .set_delay(state.config.borrow().power_source_debounce_ms);
            if let PowerObservation::Known(on_battery) = observation {
                // Keep manual profile selections associated with the currently
                // observed source even while auto-switch debounce is pending.
                state.on_battery.set(on_battery);
            } else {
                tracing::debug!("power source is temporarily unknown");
            }
            let action = debouncer.borrow_mut().observe(observation, now);
            handle_debounce_action(&state, &debouncer, action);
        }
    });
}

fn handle_debounce_action(
    state: &Rc<AppState>,
    debouncer: &Rc<std::cell::RefCell<PowerDebouncer>>,
    action: DebounceAction,
) {
    match action {
        DebounceAction::Ignored => {}
        DebounceAction::Confirmed(on_battery) => {
            on_confirmed_transition(state, on_battery);
        }
        DebounceAction::Wait(timer) => {
            schedule_confirmation(state, debouncer, timer);
        }
    }
}

fn schedule_confirmation(
    state: &Rc<AppState>,
    debouncer: &Rc<std::cell::RefCell<PowerDebouncer>>,
    timer: ConfirmationTimer,
) {
    let delay = timer.deadline_ms.saturating_sub(monotonic_millis());
    let state = state.clone();
    let debouncer = debouncer.clone();
    glib::timeout_add_local_once(Duration::from_millis(delay), move || {
        let action = debouncer.borrow_mut().on_timer(timer, monotonic_millis());
        handle_debounce_action(&state, &debouncer, action);
    });
}

fn monotonic_millis() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let elapsed = START.get_or_init(Instant::now).elapsed().as_millis();
    u64::try_from(elapsed).unwrap_or(u64::MAX)
}

fn on_confirmed_transition(state: &Rc<AppState>, on_battery: bool) {
    state.on_battery.set(on_battery);
    state.apply_panel_overdrive_policy();
    if !state.config.borrow().auto_switch_on_power_source {
        return;
    }
    let id = if on_battery {
        state.config.borrow().last_profile_on_battery.clone()
    } else {
        state.config.borrow().last_profile_on_ac.clone()
    };
    if state.active_profile_id() == id {
        return;
    }
    let name = state
        .config
        .borrow()
        .find(&id)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| id.clone());
    tracing::info!(%id, on_battery, "applying remembered power-source profile");
    // Power-source automation changes only the active profile.  The profile
    // editor's explicit target remains untouched.
    state.activate_profile(&id);
    state.apply_active();
    if state.config.borrow().show_hud {
        hud::show(state, &name, on_battery);
    }
}

/// The daemon has sampled the source on both sides of suspend. Treat its
/// post-resume value as already debounced so source-specific profile and panel
/// policies are repaired immediately instead of waiting for the event owner.
pub fn on_resume(state: &Rc<AppState>, on_battery: bool) {
    tracing::info!(on_battery, "power source changed during suspend");
    let resumed = COORDINATOR.with(|coordinator| coordinator.borrow().clone());
    if let Some(debouncer) = resumed {
        let action = debouncer.borrow_mut().force_confirm(on_battery);
        handle_debounce_action(state, &debouncer, action);
    } else {
        // This is only reachable if a resume event races application startup.
        on_confirmed_transition(state, on_battery);
    }
}

fn publish(tx: &async_channel::Sender<PowerObservation>, observation: PowerObservation) -> bool {
    tx.force_send(observation).is_ok()
}

fn upower_watch(tx: async_channel::Sender<PowerObservation>) {
    let mut reconnect = ReconnectState::default();
    loop {
        match upower_session(&tx) {
            SessionResult::ReceiverClosed => return,
            SessionResult::Disconnected { established, error } => {
                tracing::warn!(%error, "UPower connection lost; attempting reconnect");
                if reconnect.should_sample(established) {
                    let observation = sysfs_on_battery()
                        .map(PowerObservation::Known)
                        .unwrap_or(PowerObservation::Unknown);
                    if !publish(&tx, observation) {
                        return;
                    }
                }
                if tx.is_closed() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ReconnectState {
    fallback_sampled: bool,
}

impl ReconnectState {
    /// Return true exactly once for an outage. A session that was established
    /// starts a fresh outage; repeated failed reconnect attempts do not turn
    /// the fallback into a polling loop.
    fn should_sample(&mut self, established: bool) -> bool {
        if established {
            self.fallback_sampled = false;
        }
        if self.fallback_sampled {
            false
        } else {
            self.fallback_sampled = true;
            true
        }
    }
}

enum SessionResult {
    ReceiverClosed,
    Disconnected { established: bool, error: String },
}

fn upower_session(tx: &async_channel::Sender<PowerObservation>) -> SessionResult {
    let conn = match zbus::blocking::Connection::system() {
        Ok(conn) => conn,
        Err(error) => {
            return SessionResult::Disconnected {
                established: false,
                error: error.to_string(),
            };
        }
    };
    let proxy = match zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower",
        "org.freedesktop.UPower",
    ) {
        Ok(proxy) => proxy,
        Err(error) => {
            return SessionResult::Disconnected {
                established: false,
                error: error.to_string(),
            };
        }
    };

    // Register the signal stream before the one startup read so a transition
    // cannot be missed between initialization and listening. The read is only
    // a startup/cache-health check; steady state is PropertiesChanged only.
    let changed = proxy.receive_property_changed::<bool>("OnBattery");
    let initial = match proxy.get_property::<bool>("OnBattery") {
        Ok(value) => value,
        Err(error) => {
            return SessionResult::Disconnected {
                established: false,
                error: error.to_string(),
            };
        }
    };
    if !publish(tx, PowerObservation::Known(initial)) {
        return SessionResult::ReceiverClosed;
    }

    for changed in changed {
        match changed.get() {
            Ok(value) => {
                if !publish(tx, PowerObservation::Known(value)) {
                    return SessionResult::ReceiverClosed;
                }
            }
            Err(error) => {
                let _ = publish(tx, PowerObservation::Unknown);
                return SessionResult::Disconnected {
                    established: true,
                    error: error.to_string(),
                };
            }
        }
    }
    SessionResult::Disconnected {
        established: true,
        error: "UPower property stream ended".into(),
    }
}

fn sysfs_on_battery() -> Option<bool> {
    let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
        return None;
    };
    let mut supplies = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        supplies.push(SupplyReading {
            kind: std::fs::read_to_string(path.join("type")).ok(),
            online: std::fs::read_to_string(path.join("online")).ok(),
            status: std::fs::read_to_string(path.join("status")).ok(),
        });
    }
    classify_supplies(&supplies)
}

#[derive(Debug, Eq, PartialEq)]
struct SupplyReading {
    kind: Option<String>,
    online: Option<String>,
    status: Option<String>,
}

fn classify_supplies(supplies: &[SupplyReading]) -> Option<bool> {
    let adapters: Vec<&SupplyReading> = supplies
        .iter()
        .filter(|supply| matches!(supply.kind.as_deref().map(str::trim), Some("Mains" | "ADP")))
        .collect();
    if !adapters.is_empty() {
        for adapter in &adapters {
            match adapter.online.as_deref().map(str::trim) {
                Some("1") => return Some(false),
                Some("0") => {}
                _ => return None,
            }
        }
        return Some(true);
    }

    let batteries: Vec<&SupplyReading> = supplies
        .iter()
        .filter(|supply| supply.kind.as_deref().map(str::trim) == Some("Battery"))
        .collect();
    if batteries.is_empty() {
        return None;
    }
    let mut saw_known = false;
    for battery in batteries {
        match battery.status.as_deref().map(str::trim) {
            Some("Discharging") => return Some(true),
            Some("Charging" | "Full" | "Not charging") => saw_known = true,
            _ => return None,
        }
    }
    saw_known.then_some(false)
}

#[cfg(test)]
mod tests {
    use super::{ReconnectState, SupplyReading, classify_supplies, publish};
    use z13helper_core::debounce::{DebounceAction, PowerDebouncer, PowerObservation};

    fn supply(kind: &str, online: Option<&str>, status: Option<&str>) -> SupplyReading {
        SupplyReading {
            kind: Some(kind.into()),
            online: online.map(str::to_owned),
            status: status.map(str::to_owned),
        }
    }

    #[test]
    fn signal_delivery_is_bounded_and_keeps_latest_value() {
        let (tx, rx) = async_channel::bounded(1);
        assert!(publish(&tx, PowerObservation::Known(false)));
        assert!(publish(&tx, PowerObservation::Known(true)));
        assert_eq!(rx.try_recv(), Ok(PowerObservation::Known(true)));
    }

    #[test]
    fn fake_properties_changed_signal_confirms_one_stable_value() {
        let mut debouncer = PowerDebouncer::new(100);
        let timer = match debouncer.observe(PowerObservation::Known(true), 10) {
            DebounceAction::Wait(timer) => timer,
            other => panic!("expected timer, got {other:?}"),
        };
        assert_eq!(
            debouncer.observe(PowerObservation::Known(true), 50),
            DebounceAction::Wait(timer)
        );
        assert_eq!(
            debouncer.on_timer(timer, 110),
            DebounceAction::Confirmed(true)
        );
    }

    #[test]
    fn reconnect_samples_once_until_a_session_is_established() {
        let mut state = ReconnectState::default();
        assert!(state.should_sample(false));
        assert!(!state.should_sample(false));
        assert!(!state.should_sample(false));
        assert!(state.should_sample(true));
        assert!(!state.should_sample(false));
    }

    #[test]
    fn sysfs_fake_is_unknown_for_missing_or_malformed_power_state() {
        assert_eq!(classify_supplies(&[]), None);
        assert_eq!(
            classify_supplies(&[supply("Mains", Some("wat"), None)]),
            None
        );
        assert_eq!(
            classify_supplies(&[supply("Battery", None, Some("Unknown"))]),
            None
        );
    }

    #[test]
    fn sysfs_fake_uses_all_line_power_supplies() {
        assert_eq!(
            classify_supplies(&[
                supply("Mains", Some("0"), None),
                supply("Mains", Some("1"), None),
                supply("Battery", None, Some("Charging")),
            ]),
            Some(false)
        );
        assert_eq!(
            classify_supplies(&[
                supply("Mains", Some("0"), None),
                supply("Mains", Some("0"), None),
            ]),
            Some(true)
        );
    }
}
