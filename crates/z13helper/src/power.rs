//! Power-source detection: UPower D-Bus primary, sysfs fallback.
//!
//! Debounced transitions apply the remembered per-source profile and show the HUD.

use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use z13helper_core::debounce::PowerDebouncer;

use crate::app::AppState;
use crate::ui::hud;

/// Start watching power source. Prefers UPower on a background thread;
/// falls back to polling `/sys/class/power_supply` every second.
pub fn start(state: &Rc<AppState>) {
    let (tx, rx) = async_channel::unbounded::<bool>();

    // Background UPower watcher (best-effort).
    std::thread::spawn(move || {
        if let Err(e) = upower_watch(tx.clone()) {
            tracing::warn!(%e, "UPower unavailable; using sysfs fallback");
            // Seed with current sysfs value then poll.
            let _ = tx.send_blocking(sysfs_on_battery());
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let _ = tx.send_blocking(sysfs_on_battery());
            }
        }
    });

    let debouncer = Rc::new(std::cell::RefCell::new(PowerDebouncer::new(
        state.config.borrow().power_source_debounce_ms,
    )));
    let state = state.clone();

    glib::MainContext::default().spawn_local(async move {
        while let Ok(on_battery) = rx.recv().await {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            // Refresh delay from config in case it changed.
            debouncer
                .borrow_mut()
                .set_delay(state.config.borrow().power_source_debounce_ms);
            if let Some(on_battery) = debouncer.borrow_mut().on_signal(on_battery, now) {
                on_confirmed_transition(&state, on_battery);
            }
        }
    });
}

fn on_confirmed_transition(state: &Rc<AppState>, on_battery: bool) {
    if !state.config.borrow().auto_switch_on_power_source {
        return;
    }
    let id = if on_battery {
        state.config.borrow().last_profile_on_battery.clone()
    } else {
        state.config.borrow().last_profile_on_ac.clone()
    };
    if state.config.borrow().active_profile == id {
        return;
    }
    let name = state
        .config
        .borrow()
        .find(&id)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| id.clone());
    state.config.borrow_mut().active_profile = id;
    // notify=true: HUD should show (non-button path).
    state.apply_active(true);
    if state.config.borrow().show_hud {
        hud::show(&state.app, &name, on_battery);
    }
}

fn upower_watch(tx: async_channel::Sender<bool>) -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::blocking::Connection::system()?;
    // Initial read.
    let on_battery: bool = conn
        .call_method(
            Some("org.freedesktop.UPower"),
            "/org/freedesktop/UPower",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.UPower", "OnBattery"),
        )?
        .body()
        .deserialize::<zbus::zvariant::OwnedValue>()?
        .try_into()
        .map_err(|e: zbus::zvariant::Error| e.to_string())?;
    let _ = tx.send_blocking(on_battery);

    // Poll Properties via periodic Get — simpler and reliable vs signal proxy setup.
    // (A full PropertiesChanged subscription is nicer but heavier to wire with zbus 5.)
    let mut last = on_battery;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let Ok(reply) = conn.call_method(
            Some("org.freedesktop.UPower"),
            "/org/freedesktop/UPower",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.UPower", "OnBattery"),
        ) else {
            continue;
        };
        let Ok(value) = reply.body().deserialize::<zbus::zvariant::OwnedValue>() else {
            continue;
        };
        let Ok(current) = bool::try_from(value) else {
            continue;
        };
        if current != last {
            last = current;
            let _ = tx.send_blocking(current);
        }
    }
}

fn sysfs_on_battery() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
        if kind.trim() != "Mains" && kind.trim() != "ADP" {
            // Prefer AC adapter online status when present.
            continue;
        }
        let online = std::fs::read_to_string(path.join("online")).unwrap_or_default();
        return online.trim() != "1";
    }
    // Fallback: Battery status == Discharging.
    let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
        return false;
    };
    for entry in entries.flatten() {
        let kind = std::fs::read_to_string(entry.path().join("type")).unwrap_or_default();
        if kind.trim() == "Battery" {
            let status = std::fs::read_to_string(entry.path().join("status")).unwrap_or_default();
            return status.trim() == "Discharging";
        }
    }
    false
}
