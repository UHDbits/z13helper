use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::prelude::*;
use libadwaita as adw;
use z13helper_client::Client;
use z13helper_core::{ApplyRequest, Config};

use crate::services::{subscribe, worker};
use crate::ui::main_window;

/// UI-owned state. The client is cloneable; every daemon call is moved to a
/// worker thread by the individual views.
pub struct AppState {
    pub app: adw::Application,
    pub config: RefCell<Config>,
    pub config_path: PathBuf,
    config_writable: Cell<bool>,
    pub client: Client,
    pub applying: Cell<bool>,
    pub on_battery: Cell<bool>,
    pub undervolt_available: Cell<Option<bool>>,
    pub main_window: RefCell<Option<adw::ApplicationWindow>>,
    pub error_banner: RefCell<Option<adw::Banner>>,
    pending_error: RefCell<Option<String>>,
}

impl AppState {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let config_path = Config::default_path();
        let (config, pending_error, config_writable) = match Config::load_or_default(&config_path) {
            Ok(config) => (config, None, true),
            Err(error) => {
                tracing::error!(%error, "could not load config; using in-memory defaults");
                (
                    Config::default(),
                    Some(format!(
                        "Configuration was preserved and locked because it could not be loaded: {error}"
                    )),
                    false,
                )
            }
        };
        Rc::new(Self {
            app: app.clone(),
            config: RefCell::new(config),
            config_path,
            config_writable: Cell::new(config_writable),
            client: Client::new(),
            applying: Cell::new(false),
            on_battery: Cell::new(false),
            undervolt_available: Cell::new(None),
            main_window: RefCell::new(None),
            error_banner: RefCell::new(None),
            pending_error: RefCell::new(pending_error),
        })
    }

    pub fn save_config(&self) {
        if !self.config_writable.get() {
            self.report_error(
                "Configuration changes cannot be saved until the unsupported file is moved away",
            );
            return;
        }
        if let Err(error) = self.config.borrow().save(&self.config_path) {
            self.report_error(&format!("Could not save config: {error}"));
        }
    }

    /// Apply the active profile. `notify` is reserved for HUD callers; button
    /// clicks pass `false` (G-Helper convention — the highlight is enough).
    pub fn apply_active(self: &Rc<Self>, _notify: bool) {
        if self.applying.replace(true) {
            return;
        }
        let profile = self.config.borrow().active().cloned();
        let floor = self.config.borrow().fan_floor;
        let client = self.client.clone();
        let done = self.clone();
        let on_battery = self.on_battery.get();
        worker::blocking(
            move || {
                profile.map(|profile| client.apply(ApplyRequest::from_profile(&profile, floor)))
            },
            move |result| {
                done.applying.set(false);
                match result {
                    Some(Ok(response)) => {
                        let id = done.config.borrow().active_profile.clone();
                        done.config
                            .borrow_mut()
                            .set_active_for_power_source(&id, on_battery);
                        done.save_config();
                        if response.warnings.is_empty() {
                            done.clear_error();
                        } else {
                            done.report_error(&response.warnings.join(" · "));
                        }
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
            tracing::error!(%message, "z13helper error");
            *self.pending_error.borrow_mut() = Some(message.into());
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
        if let Some(message) = self.pending_error.borrow_mut().take() {
            self.report_error(&message);
        }
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
            present_from_hardware_button(window);
        }
    }
}

fn present_from_hardware_button(window: &adw::ApplicationWindow) {
    window.present();

    // The surface may not be mapped until the next main-loop iteration. Once
    // it exists, repeat the explicit toplevel focus request so an opened
    // window is raised instead of remaining underneath another application.
    let window = window.downgrade();
    glib::idle_add_local_once(move || {
        let Some(window) = window.upgrade() else {
            return;
        };
        let Some(surface) = window.surface() else {
            return;
        };
        let Ok(toplevel) = surface.downcast::<gdk4::Toplevel>() else {
            return;
        };
        toplevel.focus(gdk4::CURRENT_TIME);
    });
}
