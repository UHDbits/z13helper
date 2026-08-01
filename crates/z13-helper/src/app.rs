use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::prelude::*;
use libadwaita as adw;
use z13_helper_core::Config;
use z13ctl_client::Client;

use crate::{main_window, subscribe, worker};

/// UI-owned state. The client is cloneable; every daemon call is moved to a
/// worker thread by the individual views.
pub struct AppState {
    pub app: adw::Application,
    pub config: RefCell<Config>,
    pub config_path: PathBuf,
    pub client: Client,
    pub applying: Cell<bool>,
    pub on_battery: Cell<bool>,
    pub main_window: RefCell<Option<adw::ApplicationWindow>>,
    pub error_banner: RefCell<Option<adw::Banner>>,
}

impl AppState {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let config_path = Config::default_path();
        let config = Config::load_or_default(&config_path).unwrap_or_else(|error| {
            eprintln!("Could not load config: {error}");
            Config::default()
        });
        Rc::new(Self {
            app: app.clone(),
            config: RefCell::new(config),
            config_path,
            client: Client::new(),
            applying: Cell::new(false),
            on_battery: Cell::new(false),
            main_window: RefCell::new(None),
            error_banner: RefCell::new(None),
        })
    }

    pub fn save_config(&self) {
        if let Err(error) = self.config.borrow().save(&self.config_path) {
            eprintln!("Could not save config: {error}");
        }
    }

    /// Apply the active profile. `notify` is reserved for HUD callers; button
    /// clicks pass `false` (G-Helper convention — the highlight is enough).
    pub fn apply_active(self: &Rc<Self>, _notify: bool) {
        if self.applying.replace(true) {
            return;
        }
        let profile = self.config.borrow().active().cloned();
        let client = self.client.clone();
        let done = self.clone();
        let on_battery = self.on_battery.get();
        worker::blocking(
            move || {
                let available = client
                    .get_state()
                    .map(|s| s.undervolt_available)
                    .unwrap_or(false);
                profile.map(|profile| {
                    z13_helper_core::apply_profile(
                        &z13_helper_core::ClientDaemon(&client),
                        &profile,
                        available,
                    )
                })
            },
            move |result| {
                done.applying.set(false);
                match result {
                    Some(Ok(())) => {
                        let id = done.config.borrow().active_profile.clone();
                        done.config
                            .borrow_mut()
                            .set_active_for_power_source(&id, on_battery);
                        done.save_config();
                        done.clear_error();
                    }
                    Some(Err(error)) => done.report_error(&error.to_string()),
                    None => {}
                }
            },
        );
    }

    pub fn report_error(&self, message: &str) {
        if let Some(banner) = self.error_banner.borrow().as_ref() {
            banner.set_title(message);
            banner.set_revealed(true);
        } else {
            eprintln!("{message}");
        }
    }

    pub fn clear_error(&self) {
        if let Some(banner) = self.error_banner.borrow().as_ref() {
            banner.set_revealed(false);
        }
    }

    pub fn activate(self: &Rc<Self>) {
        if let Some(window) = self.main_window.borrow().as_ref() {
            window.present();
            return;
        }
        let window = main_window::build(self);
        *self.main_window.borrow_mut() = Some(window.clone());
        subscribe::start(self);
        window.present();
    }

    pub fn toggle_window(&self) {
        let windows = self.main_window.borrow();
        let Some(window) = windows.as_ref() else {
            return;
        };
        if window.is_visible() {
            window.set_visible(false);
        } else {
            window.present();
        }
    }
}
