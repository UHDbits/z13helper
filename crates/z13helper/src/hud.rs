//! G-Helper-style HUD toast for power-source / profile switches.
//!
//! Prefer gtk4-layer-shell on Wayland (click-through via empty input region).
//! Fall back to org.freedesktop.Notifications.

use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

pub fn show(app: &adw::Application, profile_name: &str, on_battery: bool) {
    let glyph = if on_battery { "🔋" } else { "🔌" };
    let message = format!("{glyph}  {profile_name}");

    if try_popup(app, &message) {
        return;
    }
    notify_fallback(&message);
}

fn try_popup(app: &adw::Application, message: &str) -> bool {
    let window = gtk::Window::builder()
        .application(app.upcast_ref::<gtk::Application>())
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

    #[cfg(feature = "layer-shell")]
    {
        use gtk4_layer_shell::{Edge, KeyboardMode, Layer};
        if gtk4_layer_shell::is_supported() {
            gtk4_layer_shell::init_for_window(&window);
            gtk4_layer_shell::set_layer(&window, Layer::Overlay);
            gtk4_layer_shell::set_anchor(&window, Edge::Bottom, true);
            gtk4_layer_shell::set_margin(&window, Edge::Bottom, 80);
            gtk4_layer_shell::set_keyboard_mode(&window, KeyboardMode::None);
            gtk4_layer_shell::set_exclusive_zone(&window, -1);
        }
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
