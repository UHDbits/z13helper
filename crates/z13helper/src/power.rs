//! Power-source detection: UPower D-Bus primary, sysfs while UPower is down.
//!
//! Debounced transitions apply the remembered per-source profile and show the HUD.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use z13helper_core::debounce::{
    ConfirmationTimer, DebounceAction, PowerDebouncer, PowerObservation,
};

use crate::app::AppState;
use crate::hud;

/// Start watching power source. The D-Bus owner lives for the UI process and
/// exits when its bounded GLib delivery channel is closed. While UPower is
/// unavailable, sysfs is sampled once per backed-off reconnect attempt.
/// Returns the GTK-thread debouncer that [`on_resume`] must share.
pub fn start(state: &Rc<AppState>) -> Rc<RefCell<PowerDebouncer>> {
    // Power state is a snapshot, not an event log. Keep only the newest value
    // if GTK is temporarily busy.
    let (tx, rx) = async_channel::bounded::<PowerObservation>(1);

    // Background UPower watcher with a sysfs sample per failed reconnect.
    let watcher_tx = tx.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("z13helper-power".into())
        .spawn(move || {
            upower_watch(watcher_tx);
        })
    {
        tracing::error!(%error, "could not start power-source watcher");
        let _ = tx.force_send(PowerObservation::Unknown);
    }
    drop(tx);

    let debouncer = Rc::new(RefCell::new(PowerDebouncer::new(
        state.config.borrow().power_source_debounce_ms,
    )));
    let state = state.clone();
    let for_resume = debouncer.clone();

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
    for_resume
}

fn handle_debounce_action(
    state: &Rc<AppState>,
    debouncer: &Rc<RefCell<PowerDebouncer>>,
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
    debouncer: &Rc<RefCell<PowerDebouncer>>,
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
    glib::monotonic_time() as u64 / 1000
}

fn on_confirmed_transition(state: &Rc<AppState>, on_battery: bool) {
    state.on_battery.set(on_battery);
    state.apply_panel_overdrive_policy();
    let id = {
        let config = state.config.borrow();
        if on_battery {
            config.last_profile_on_battery.clone()
        } else {
            config.last_profile_on_ac.clone()
        }
    };
    if !state.config.borrow().auto_switch_on_power_source || state.active_profile_id() == id {
        // Nothing to switch, but the first confirmation after startup is
        // also when a daemon that lost its state is noticed and repaired.
        state.reapply_if_daemon_differs();
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
    select_profile(state, &id);
    if state.config.borrow().show_hud {
        hud::show(state, &name, on_battery);
    }
}

/// Make `id` the active profile and apply it if that changed anything.
pub fn select_profile(state: &Rc<AppState>, id: &str) {
    if state.activate_profile(id) {
        state.apply_active();
    }
}

/// The daemon has sampled the source on both sides of suspend. Treat its
/// post-resume value as already debounced so source-specific profile and panel
/// policies are repaired immediately instead of waiting for the event owner.
pub fn on_resume(state: &Rc<AppState>, debouncer: &Rc<RefCell<PowerDebouncer>>, on_battery: bool) {
    tracing::info!(on_battery, "power source changed during suspend");
    let action = debouncer.borrow_mut().force_confirm(on_battery);
    handle_debounce_action(state, debouncer, action);
}

fn upower_watch(tx: async_channel::Sender<PowerObservation>) {
    let mut reconnect = ReconnectState::default();
    loop {
        match upower_session(&tx) {
            SessionResult::ReceiverClosed => return,
            SessionResult::Disconnected { established, error } => {
                let delay = reconnect.next_delay(established);
                if delay == UPOWER_RETRY_MIN {
                    tracing::warn!(%error, "UPower unavailable; using sysfs until it returns");
                } else {
                    tracing::debug!(%error, ?delay, "UPower still unavailable");
                }
                // UPower is the only change notifier, so keep sampling the
                // read-only fallback until it returns.
                let observation = sysfs_on_battery()
                    .map(PowerObservation::Known)
                    .unwrap_or(PowerObservation::Unknown);
                if tx.force_send(observation).is_err() {
                    return;
                }
                std::thread::sleep(delay);
            }
        }
    }
}

const UPOWER_RETRY_MIN: Duration = Duration::from_secs(1);
const UPOWER_RETRY_MAX: Duration = Duration::from_secs(15);

#[derive(Default)]
struct ReconnectState {
    delay: Option<Duration>,
}

impl ReconnectState {
    /// Delay before the next UPower attempt. It doubles while UPower stays
    /// unavailable; an established session starts a fresh outage.
    fn next_delay(&mut self, established: bool) -> Duration {
        let delay = match self.delay {
            Some(delay) if !established => (delay * 2).min(UPOWER_RETRY_MAX),
            _ => UPOWER_RETRY_MIN,
        };
        self.delay = Some(delay);
        delay
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
    if tx.force_send(PowerObservation::Known(initial)).is_err() {
        return SessionResult::ReceiverClosed;
    }

    for changed in changed {
        match changed.get() {
            Ok(value) => {
                if tx.force_send(PowerObservation::Known(value)).is_err() {
                    return SessionResult::ReceiverClosed;
                }
            }
            Err(error) => {
                let _ = tx.force_send(PowerObservation::Unknown);
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
            scope: std::fs::read_to_string(path.join("scope")).ok(),
        });
    }
    classify_supplies(&supplies)
}

#[derive(Debug, Eq, PartialEq)]
struct SupplyReading {
    kind: Option<String>,
    online: Option<String>,
    status: Option<String>,
    scope: Option<String>,
}

/// Entries can vanish mid-scan, so a missing `online`/`status` file skips
/// that supply instead of making the whole sample unknown.
fn classify_supplies(supplies: &[SupplyReading]) -> Option<bool> {
    fn field(value: &Option<String>) -> Option<&str> {
        value.as_deref().map(str::trim)
    }
    let mut found_adapter = false;
    for supply in supplies {
        if field(&supply.kind) != Some("Mains") {
            continue;
        }
        match field(&supply.online) {
            Some("1") => return Some(false),
            Some("0") => found_adapter = true,
            None => {}
            _ => return None,
        }
    }
    if found_adapter {
        return Some(true);
    }

    let mut saw_known = false;
    for supply in supplies {
        // Peripheral (HID) batteries report scope "Device", not the system.
        if field(&supply.kind) != Some("Battery") || field(&supply.scope) == Some("Device") {
            continue;
        }
        match field(&supply.status) {
            Some("Discharging") => return Some(true),
            Some("Charging" | "Full" | "Not charging") => saw_known = true,
            None => {}
            _ => return None,
        }
    }
    saw_known.then_some(false)
}

#[cfg(test)]
mod tests {
    use super::{ReconnectState, SupplyReading, classify_supplies};
    use std::time::Duration;

    fn supply(kind: &str, online: Option<&str>, status: Option<&str>) -> SupplyReading {
        SupplyReading {
            kind: Some(kind.into()),
            online: online.map(str::to_owned),
            status: status.map(str::to_owned),
            scope: None,
        }
    }

    #[test]
    fn reconnect_backs_off_until_a_session_is_established() {
        let mut state = ReconnectState::default();
        let delays: Vec<u64> = (0..6).map(|_| state.next_delay(false).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 15, 15]);
        assert_eq!(state.next_delay(true), Duration::from_secs(1));
        assert_eq!(state.next_delay(false), Duration::from_secs(2));
    }

    #[test]
    fn classify_supplies_ignores_peripherals_and_needs_a_definite_state() {
        let mut stylus = supply("Battery", None, Some("Discharging"));
        stylus.scope = Some("Device\n".into());
        let cases = [
            // Peripheral batteries and vanished entries are skipped.
            (
                vec![stylus, supply("Battery", None, Some("Charging"))],
                Some(false),
            ),
            (
                vec![
                    supply("Mains", None, None),
                    supply("Battery", None, None),
                    supply("Battery", None, Some("Discharging")),
                ],
                Some(true),
            ),
            // Any online line-power supply means AC.
            (
                vec![
                    supply("Mains", Some("0"), None),
                    supply("Mains", Some("1"), None),
                    supply("Battery", None, Some("Charging")),
                ],
                Some(false),
            ),
            (
                vec![
                    supply("Mains", Some("0"), None),
                    supply("Mains", Some("0"), None),
                ],
                Some(true),
            ),
            // Missing or malformed state is unknown, not a guess.
            (vec![], None),
            (vec![supply("Mains", Some("wat"), None)], None),
            (vec![supply("Battery", None, Some("Unknown"))], None),
        ];
        for (supplies, on_battery) in cases {
            assert_eq!(classify_supplies(&supplies), on_battery, "{supplies:?}");
        }
    }
}
