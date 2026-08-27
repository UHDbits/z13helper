//! z13helper — G-Helper-style control panel for z13helperd.
mod app;
mod css;
mod gamescope;
mod profile_coordinator;
mod resources;
mod services;
mod ui;

use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const APPLICATION_ID: &str = "com.ashtonantila.z13helper";

fn main() {
    gamescope::select_gdk_backend();
    let prefer_dark = consume_legacy_dark_preference();
    let app = adw::Application::new(Some(APPLICATION_ID), gio::ApplicationFlags::empty());
    if prefer_dark {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::PreferDark);
    }
    app.connect_startup(|_| {
        resources::register();
        css::install();
    });
    install_standard_actions(&app);
    let state = Rc::new(RefCell::new(None));
    let first_activation = Rc::new(Cell::new(true));
    let start_hidden = std::env::var_os("Z13HELPER_START_HIDDEN").is_some();
    app.connect_activate(move |app| {
        let state = state
            .borrow_mut()
            .get_or_insert_with(|| {
                let state = app::AppState::new(app);
                services::power::start(&state);
                state
            })
            .clone();
        let show = !start_hidden || !first_activation.replace(false);
        state.activate(show);
    });
    app.run();
}

fn consume_legacy_dark_preference() -> bool {
    gtk4::init().expect("GTK could not connect to the display");
    let Some(settings) = gtk4::Settings::default() else {
        return false;
    };
    let prefer_dark = settings.is_gtk_application_prefer_dark_theme();
    if prefer_dark {
        settings.set_gtk_application_prefer_dark_theme(false);
    }
    prefer_dark
}

fn install_standard_actions(app: &adw::Application) {
    let close = gio::SimpleAction::new("close", None);
    let weak_app = app.downgrade();
    close.connect_activate(move |_, _| {
        if let Some(window) = weak_app.upgrade().and_then(|app| app.active_window()) {
            window.close();
        }
    });
    app.add_action(&close);
    app.set_accels_for_action("app.close", &["<Primary>w"]);

    let hide_all = gio::SimpleAction::new("hide-all", None);
    let weak_app = app.downgrade();
    hide_all.connect_activate(move |_, _| {
        if let Some(app) = weak_app.upgrade() {
            for window in app.windows() {
                window.close();
            }
        }
    });
    app.add_action(&hide_all);
    app.set_accels_for_action("app.hide-all", &["<Primary>q"]);
}
