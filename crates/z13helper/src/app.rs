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
    apply_pending: Cell<bool>,
    pub on_battery: Cell<bool>,
    pub undervolt_available: Cell<Option<bool>>,
    pub main_window: RefCell<Option<adw::ApplicationWindow>>,
    main_window_visible: Cell<bool>,
    pub persistent_banner: RefCell<Option<adw::Banner>>,
    toast_overlays: RefCell<Vec<glib::WeakRef<adw::ToastOverlay>>>,
    pending_persistent_error: RefCell<Option<String>>,
    pending_toast: RefCell<Option<String>>,
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
            apply_pending: Cell::new(false),
            on_battery: Cell::new(false),
            undervolt_available: Cell::new(None),
            main_window: RefCell::new(None),
            main_window_visible: Cell::new(false),
            persistent_banner: RefCell::new(None),
            toast_overlays: RefCell::new(Vec::new()),
            pending_persistent_error: RefCell::new(pending_error),
            pending_toast: RefCell::new(None),
        })
    }

    pub fn save_config(&self) {
        if !self.config_writable.get() {
            self.report_persistent_error(
                "Configuration changes cannot be saved until the unsupported file is moved away",
            );
            return;
        }
        if let Err(error) = self.config.borrow().save(&self.config_path) {
            self.report_error(&format!("Could not save config: {error}"));
        }
    }

    pub fn apply_panel_overdrive_policy(self: &Rc<Self>) {
        let enabled = self
            .config
            .borrow()
            .panel_overdrive_enabled(self.on_battery.get());
        let client = self.client.clone();
        let feedback = self.clone();
        worker::blocking(
            move || client.panel_overdrive_set(i32::from(enabled)),
            move |result| {
                if let Err(error) = result {
                    feedback.report_error(&format!("Panel overdrive failed: {error}"));
                }
            },
        );
    }

    fn seed_factory_fan_curves(self: &Rc<Self>) {
        if self
            .config
            .borrow()
            .profiles
            .iter()
            .filter(|profile| profile.builtin)
            .all(|profile| profile.factory_fan_curves_loaded || profile.apply_fan_curve)
        {
            return;
        }
        let mut ppd_profiles: Vec<String> = self
            .config
            .borrow()
            .profiles
            .iter()
            .filter(|profile| {
                profile.builtin && !profile.factory_fan_curves_loaded && !profile.apply_fan_curve
            })
            .filter_map(|profile| profile.ppd_profile.clone())
            .collect();
        ppd_profiles.sort();
        ppd_profiles.dedup();
        let client = self.client.clone();
        let done = self.clone();
        worker::blocking(
            move || client.factory_fan_curves(ppd_profiles),
            move |result| match result {
                Ok(curves) => {
                    let mut config = done.config.borrow_mut();
                    for profile in &mut config.profiles {
                        if !profile.builtin || profile.apply_fan_curve {
                            continue;
                        }
                        if let Some(curve) =
                            profile.ppd_profile.as_ref().and_then(|ppd| curves.get(ppd))
                        {
                            profile.fan_curves = *curve;
                            profile.factory_fan_curves_loaded = true;
                        }
                    }
                    drop(config);
                    done.save_config();
                }
                Err(error) => done.report_error(&format!(
                    "Could not read firmware factory fan curves; using bundled defaults: {error}"
                )),
            },
        );
    }

    /// Apply the active profile. `notify` is reserved for HUD callers; button
    /// clicks pass `false` (G-Helper convention — the highlight is enough).
    pub fn apply_active(self: &Rc<Self>, _notify: bool) {
        if self.applying.replace(true) {
            self.apply_pending.set(true);
            return;
        }
        let profile = self.config.borrow().active().cloned();
        let disable_high_power = self.config.borrow().disable_high_power_fan_protection;
        let client = self.client.clone();
        let done = self.clone();
        let on_battery = self.on_battery.get();
        worker::blocking(
            move || {
                profile.map(|profile| {
                    client.apply(ApplyRequest::from_profile(&profile, disable_high_power))
                })
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
                        if !response.warnings.is_empty() {
                            done.report_error(&response.warnings.join(" · "));
                        }
                    }
                    Some(Err(error)) => done.report_error(&error.to_string()),
                    None => {}
                }
                if done.apply_pending.replace(false) {
                    done.apply_active(false);
                }
            },
        );
    }

    pub fn report_error(&self, message: &str) {
        tracing::error!(%message, "z13helper error");
        let mut overlays = self.toast_overlays.borrow_mut();
        overlays.retain(|overlay| overlay.upgrade().is_some());
        let target = overlays.iter().rev().find_map(glib::WeakRef::upgrade);
        drop(overlays);
        if let Some(overlay) = target {
            overlay.add_toast(adw::Toast::new(message));
        } else {
            *self.pending_toast.borrow_mut() = Some(message.into());
        }
    }

    pub fn report_persistent_error(&self, message: &str) {
        if let Some(banner) = self.persistent_banner.borrow().as_ref() {
            banner.set_title(message);
            banner.set_revealed(true);
        } else {
            *self.pending_persistent_error.borrow_mut() = Some(message.into());
        }
    }

    pub fn register_toast_overlay(&self, overlay: &adw::ToastOverlay) {
        self.toast_overlays.borrow_mut().push(overlay.downgrade());
        if let Some(message) = self.pending_toast.borrow_mut().take() {
            overlay.add_toast(adw::Toast::new(&message));
        }
    }

    pub fn activate(self: &Rc<Self>, show: bool) {
        if self.main_window.borrow().is_some() {
            if show {
                self.show_window();
            }
            return;
        }
        let window = main_window::build(self);
        if let Some(message) = self.pending_persistent_error.borrow_mut().take() {
            self.report_persistent_error(&message);
        }
        *self.main_window.borrow_mut() = Some(window.clone());
        subscribe::start(self);
        if show {
            self.show_window();
        } else {
            self.hide_window();
        }
        self.seed_factory_fan_curves();
    }

    pub fn show_window(&self) {
        let Some(window) = self.main_window.borrow().clone() else {
            return;
        };
        self.main_window_visible.set(true);
        present_from_hardware_button(&window);
    }

    pub fn hide_window(&self) {
        let Some(window) = self.main_window.borrow().clone() else {
            return;
        };
        self.main_window_visible.set(false);
        window.set_visible(false);
    }

    pub fn toggle_window(&self) {
        if self.main_window_visible.get() {
            self.hide_window();
        } else {
            self.show_window();
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
