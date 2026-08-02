//! z13helper — G-Helper-style control panel for z13helperd.
mod app;
mod css;
mod services;
mod ui;

use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;

const APPLICATION_ID: &str = "com.ashtonantila.z13helper";

fn main() {
    // GTK_A11Y=none avoids AT-SPI D-Bus timeouts that block GTK init.
    if std::env::var_os("GTK_A11Y").is_none() {
        unsafe { std::env::set_var("GTK_A11Y", "none") };
    }
    // Gamescope's Wayland bridge cannot host this GTK surface reliably; its
    // Xwayland server can, but only when the advertised socket exists.
    if let Some(display) = std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY") {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into());
        if std::path::Path::new(&runtime).join(&display).exists() {
            unsafe { std::env::set_var("GDK_BACKEND", "x11") };
        }
    }

    let app = adw::Application::new(Some(APPLICATION_ID), gio::ApplicationFlags::empty());
    app.connect_startup(|_| css::install());
    let state = Rc::new(RefCell::new(None));
    app.connect_activate(move |app| {
        let state = state
            .borrow_mut()
            .get_or_insert_with(|| {
                let state = app::AppState::new(app);
                services::power::start(&state);
                state
            })
            .clone();
        state.activate();
    });
    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_metadata_matches_application_id() {
        let desktop = include_str!("../../../contrib/com.ashtonantila.z13helper.desktop");
        assert!(desktop.contains(&format!("StartupWMClass={APPLICATION_ID}")));
        assert!(desktop.contains(&format!("X-GNOME-Application-ID={APPLICATION_ID}")));
    }
}
