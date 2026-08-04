//! G-Helper-style HUD toast for power-source / profile switches.
//!
//! Uses gtk4-layer-shell on Wayland (click-through via empty input region).
//! Under gamescope, use its non-interactive external-overlay plane.
//! Fall back to org.freedesktop.Notifications when neither overlay path works.

use std::rc::Rc;

use gtk::prelude::*;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::app::AppState;

pub fn show(state: &Rc<AppState>, profile_name: &str, on_battery: bool) {
    let glyph = if on_battery { "🔋" } else { "🔌" };
    let message = format!("{glyph}  {profile_name}");

    if try_popup(state, &message) {
        return;
    }
    notify_fallback(&message);
}

fn try_popup(state: &Rc<AppState>, message: &str) -> bool {
    let gamescope = state.gamescope.as_ref();
    let layer_shell = gtk4_layer_shell::is_supported();
    if gamescope.is_none() && !layer_shell {
        return false;
    }

    let window = gtk::Window::builder()
        .application(state.app.upcast_ref::<gtk::Application>())
        .title("z13helper HUD")
        .default_width(300)
        .default_height(100)
        .decorated(false)
        .resizable(false)
        .build();
    window.add_css_class("hud");

    let label = gtk::Label::new(Some(message));
    label.add_css_class("hud-label");
    label.set_margin_top(24);
    label.set_margin_bottom(24);
    label.set_margin_start(24);
    label.set_margin_end(24);
    window.set_child(Some(&label));

    if let Some(gamescope) = gamescope {
        gamescope.prepare_hud(&window);
    }

    if layer_shell {
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_anchor(Edge::Bottom, true);
        window.set_margin(Edge::Bottom, 80);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(-1);
    }

    window.connect_realize(|window| {
        if let Some(surface) = window.surface() {
            // Empty input region = click-through.
            let region = gtk::cairo::Region::create();
            surface.set_input_region(Some(&region));
        }
    });

    window.present();
    glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
        window.close();
    });
    true
}

fn notify_fallback(message: &str) {
    // Best-effort sync notify via zbus in a worker thread.
    let message = message.to_string();
    std::thread::spawn(move || {
        let Ok(conn) = zbus::blocking::Connection::session() else {
            return;
        };
        let _ = conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "z13helper",
                0u32,
                "",
                "Performance Mode",
                message,
                Vec::<String>::new(),
                std::collections::HashMap::<String, zbus::zvariant::Value>::new(),
                2000i32,
            ),
        );
    });
}
