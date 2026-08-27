use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::AdwDialogExt;
use z13helper_client::Client;
use z13helper_core::{ApplyRequest, Config, ControllerAction};

use crate::css;
use crate::gamescope::Gamescope;
use crate::profile_coordinator::{
    ApplyRequest as CoordinatorApplyRequest, ApplyToken, ProfileCoordinator, ProfileOperationToken,
};
use crate::services::{controller::ControllerCapture, subscribe, worker};
use crate::ui::{hud, main_window};

type ControllerActivation = (glib::WeakRef<gtk4::Widget>, Rc<dyn Fn()>);

/// UI-owned state. The client is cloneable; every daemon call is moved to a
/// worker thread by the individual views.
pub struct AppState {
    pub app: adw::Application,
    pub config: RefCell<Config>,
    pub config_path: PathBuf,
    /// Runtime-only profile identity/revision state. Never serialized.
    pub coordinator: RefCell<ProfileCoordinator>,
    config_writable: Cell<bool>,
    pub client: Client,
    pub on_battery: Cell<bool>,
    pub undervolt_available: Cell<Option<bool>>,
    pub main_window: RefCell<Option<adw::ApplicationWindow>>,
    fans_window: RefCell<Option<gtk4::Window>>,
    main_window_visible: Cell<bool>,
    pub gamescope: Option<Rc<Gamescope>>,
    pub(crate) notification_owner: hud::NotificationOwner,
    controller_capture: Option<ControllerCapture>,
    controller_activations: RefCell<Vec<ControllerActivation>>,
    pub persistent_banner: RefCell<Option<adw::Banner>>,
    toast_overlays: RefCell<Vec<glib::WeakRef<adw::ToastOverlay>>>,
    pending_persistent_error: RefCell<Option<String>>,
    pending_toast: RefCell<Option<String>>,
}

impl AppState {
    pub fn new(app: &adw::Application) -> Rc<Self> {
        let gamescope = Gamescope::connect();
        if let Some(gamescope) = gamescope.as_ref() {
            css::install_gamescope_scale(gamescope.scale());
        }
        let config_path = Config::default_path();
        let client = Client::new();
        let controller_capture = gamescope
            .as_ref()
            .map(|_| ControllerCapture::start(client.clone()));
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
        let profile_ids = config.profiles.iter().map(|profile| profile.id.clone());
        let coordinator = ProfileCoordinator::new(config.active_profile.clone(), profile_ids);
        Rc::new(Self {
            app: app.clone(),
            config: RefCell::new(config),
            config_path,
            coordinator: RefCell::new(coordinator),
            config_writable: Cell::new(config_writable),
            client,
            on_battery: Cell::new(false),
            undervolt_available: Cell::new(None),
            main_window: RefCell::new(None),
            fans_window: RefCell::new(None),
            main_window_visible: Cell::new(false),
            gamescope,
            notification_owner: hud::NotificationOwner::start(),
            controller_capture,
            controller_activations: RefCell::new(Vec::new()),
            persistent_banner: RefCell::new(None),
            toast_overlays: RefCell::new(Vec::new()),
            pending_persistent_error: RefCell::new(pending_error),
            pending_toast: RefCell::new(None),
        })
    }

    pub fn active_profile_id(&self) -> String {
        self.coordinator.borrow().active_id().to_owned()
    }

    pub fn editing_profile_id(&self) -> String {
        self.coordinator.borrow().editing_id().to_owned()
    }

    pub fn activate_profile(&self, id: &str) -> bool {
        if self.config.borrow().find(id).is_none() {
            return false;
        }
        let changed = self.coordinator.borrow_mut().activate(id);
        if changed {
            self.config.borrow_mut().active_profile = id.into();
        }
        changed
    }

    pub fn select_editing_profile(&self, id: &str) -> bool {
        if self.config.borrow().find(id).is_none() {
            return false;
        }
        self.coordinator.borrow_mut().select_editing(id)
    }

    pub fn mark_config_changed(&self) {
        self.coordinator.borrow_mut().mark_config_changed();
    }

    /// Mutate one named profile and advance only that profile's runtime
    /// revision. All callers run on GTK's main thread.
    pub fn edit_profile<T>(
        &self,
        id: &str,
        edit: impl FnOnce(&mut z13helper_core::Profile) -> T,
    ) -> Option<T> {
        let result = self.config.borrow_mut().find_mut(id).map(edit);
        if result.is_some() {
            self.coordinator.borrow_mut().mark_profile_changed(id);
        }
        result
    }

    pub fn add_custom_profile(&self) -> String {
        let id = self.config.borrow_mut().add_custom().id.clone();
        let mut coordinator = self.coordinator.borrow_mut();
        coordinator.add_profile(&id);
        coordinator.activate(&id);
        coordinator.select_editing(&id);
        id
    }

    pub fn rename_profile(&self, id: &str, name: &str) -> bool {
        let renamed = self.config.borrow_mut().rename(id, name);
        if renamed {
            self.coordinator.borrow_mut().mark_profile_changed(id);
        }
        renamed
    }

    pub fn remove_profile(&self, id: &str) -> bool {
        let old_active = self.active_profile_id();
        let old_editing = self.editing_profile_id();
        if !self.config.borrow_mut().remove(id) {
            return false;
        }
        let config = self.config.borrow();
        let fallback_editing = if old_editing == id {
            config.active_profile.clone()
        } else {
            old_editing
        };
        let active = config.active_profile.clone();
        drop(config);
        self.coordinator
            .borrow_mut()
            .remove_profile(id, &active, &fallback_editing);
        debug_assert_eq!(old_active == id, self.active_profile_id() != old_active);
        true
    }

    pub fn begin_profile_operation(&self, id: &str) -> Option<ProfileOperationToken> {
        self.coordinator.borrow_mut().begin_operation(id)
    }

    pub fn operation_is_current(&self, token: &ProfileOperationToken) -> bool {
        self.coordinator.borrow().is_current_operation(token)
    }

    pub fn operation_is_current_editor(&self, token: &ProfileOperationToken) -> bool {
        self.coordinator.borrow().is_current_editor(token)
    }

    pub fn replace_profile_if_current(
        &self,
        token: &ProfileOperationToken,
        replacement: z13helper_core::Profile,
    ) -> bool {
        if !self.operation_is_current(token) {
            return false;
        }
        let replaced = self
            .config
            .borrow_mut()
            .find_mut(&token.id)
            .map(|profile| *profile = replacement)
            .is_some();
        if replaced {
            self.coordinator
                .borrow_mut()
                .mark_profile_changed(&token.id);
        }
        replaced
    }

    pub fn save_config(self: &Rc<Self>) {
        if !self.config_writable.get() {
            self.report_persistent_error(
                "Configuration changes cannot be saved until the unsupported file is moved away",
            );
            return;
        }
        let config = self.config.borrow().clone();
        let path = self.config_path.clone();
        let feedback = self.clone();
        worker::blocking(
            move || config.save(&path),
            move |result| {
                if let Err(error) = result {
                    feedback.report_error(&format!("Could not save config: {error}"));
                }
            },
        );
    }

    pub fn apply_panel_overdrive_policy(self: &Rc<Self>) {
        let enabled = self
            .config
            .borrow()
            .panel_overdrive_enabled(self.on_battery.get());
        let client = self.client.clone();
        let feedback = self.clone();
        worker::blocking(
            move || client.panel_overdrive_set(enabled),
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
        let targets: Vec<(ProfileOperationToken, String, String)> = self
            .config
            .borrow()
            .profiles
            .iter()
            .filter(|profile| {
                profile.builtin && !profile.factory_fan_curves_loaded && !profile.apply_fan_curve
            })
            .filter_map(|profile| {
                let ppd = profile.ppd_profile.clone()?;
                let token = self.begin_profile_operation(&profile.id)?;
                Some((token, profile.id.clone(), ppd))
            })
            .collect();
        let mut ppd_profiles: Vec<String> = targets.iter().map(|(_, _, ppd)| ppd.clone()).collect();
        ppd_profiles.sort();
        ppd_profiles.dedup();
        if targets.is_empty() {
            return;
        }
        let client = self.client.clone();
        let done = self.clone();
        worker::blocking(
            move || client.factory_fan_curves(ppd_profiles),
            move |result| match result {
                Ok(curves) => {
                    let mut changed = false;
                    let mut changed_active = false;
                    for (token, id, ppd) in &targets {
                        if !done.operation_is_current(token) {
                            if done.active_profile_id() == *id {
                                changed_active = true;
                            }
                            continue;
                        }
                        let Some(curve) = curves.get(ppd) else {
                            continue;
                        };
                        let committed = done.edit_profile(id, |profile| {
                            if profile.builtin
                                && !profile.apply_fan_curve
                                && profile.ppd_profile.as_deref() == Some(ppd.as_str())
                            {
                                profile.fan_curves = *curve;
                                profile.factory_fan_curves_loaded = true;
                                true
                            } else {
                                false
                            }
                        });
                        if committed == Some(true) {
                            changed = true;
                            changed_active |= done.active_profile_id() == *id;
                        }
                        if !done.operation_is_current(token) && done.active_profile_id() == *id {
                            // A stale read may have completed after an edit;
                            // converge hardware to the newest active intent.
                            changed_active = true;
                        }
                    }
                    if changed {
                        done.save_config();
                    }
                    if changed_active {
                        done.apply_active();
                    }
                }
                Err(error) => done.report_error(&format!(
                    "Could not read firmware factory fan curves; using bundled defaults: {error}"
                )),
            },
        );
    }

    /// Apply the active profile. Requests are serialized and the latest one is
    /// queued while an apply is in flight.
    pub fn apply_active(self: &Rc<Self>) {
        let active_id = self.active_profile_id();
        if self.config.borrow().find(&active_id).is_none() {
            return;
        }
        let request = self
            .coordinator
            .borrow_mut()
            .request_apply(self.on_battery.get());
        let CoordinatorApplyRequest::Started(token) = request else {
            return;
        };
        self.start_apply(token);
    }

    fn start_apply(self: &Rc<Self>, token: ApplyToken) {
        let profile = self.config.borrow().find(&token.id).cloned();
        let disable_high_power = self.config.borrow().disable_high_power_fan_protection;
        let client = self.client.clone();
        let done = self.clone();
        let worker_token = token.clone();
        worker::blocking(
            move || {
                profile.map(|profile| {
                    client.apply(ApplyRequest::from_profile(&profile, disable_high_power))
                })
            },
            move |result| {
                let on_battery = done.on_battery.get();
                let completion = done
                    .coordinator
                    .borrow_mut()
                    .complete_apply(&worker_token, on_battery);
                match result {
                    Some(Ok(response)) if completion.current => {
                        {
                            done.config
                                .borrow_mut()
                                .set_active_for_power_source(&worker_token.id, on_battery);
                        }
                        done.save_config();
                        if !response.warnings.is_empty() {
                            done.report_error(&response.warnings.join(" · "));
                        }
                    }
                    Some(Ok(response)) => {
                        if !response.warnings.is_empty() {
                            done.report_error(&response.warnings.join(" · "));
                        }
                    }
                    Some(Err(error)) => done.report_error(&error.to_string()),
                    None => {}
                }
                if let Some(next) = completion.next {
                    done.start_apply(next);
                }
            },
        );
    }

    pub fn report_error(&self, message: &str) {
        tracing::error!(%message, "z13helper error");
        let target = self.visible_toast_overlay();
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
        self.flush_pending_toast();
    }

    fn visible_toast_overlay(&self) -> Option<adw::ToastOverlay> {
        let main = self.main_window.borrow().clone();
        let mut overlays = self.toast_overlays.borrow_mut();
        overlays.retain(|overlay| overlay.upgrade().is_some());
        overlays.iter().rev().find_map(|overlay| {
            let overlay = overlay.upgrade()?;
            let window = overlay.root()?.downcast::<gtk4::Window>().ok()?;
            let visible = if main
                .as_ref()
                .is_some_and(|main| main.upcast_ref::<gtk4::Window>() == &window)
            {
                self.main_window_visible.get()
            } else {
                window.is_visible()
            };
            visible.then_some(overlay)
        })
    }

    fn flush_pending_toast(&self) {
        if let Some(overlay) = self.visible_toast_overlay()
            && let Some(message) = self.pending_toast.borrow_mut().take()
        {
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
        self.main_window_visible.set(show);
        if let Some(capture) = self.controller_capture.as_ref() {
            capture.set_enabled(show);
        }
        if let Some(gamescope) = self.gamescope.as_ref() {
            gamescope.prepare_main(&window, show);
        } else if show {
            present_from_hardware_button(&window);
        } else {
            window.set_visible(false);
        }
        self.seed_factory_fan_curves();
        self.flush_pending_toast();
    }

    pub fn show_window(&self) {
        let Some(window) = self.main_window.borrow().clone() else {
            return;
        };
        self.main_window_visible.set(true);
        if let Some(capture) = self.controller_capture.as_ref() {
            capture.set_enabled(true);
        }
        if let Some(gamescope) = self.gamescope.as_ref() {
            gamescope.show();
        } else {
            present_from_hardware_button(&window);
        }
        self.flush_pending_toast();
    }

    pub fn hide_window(&self) {
        let Some(window) = self.main_window.borrow().clone() else {
            return;
        };
        self.main_window_visible.set(false);
        if let Some(capture) = self.controller_capture.as_ref() {
            capture.set_enabled(false);
        }
        if let Some(gamescope) = self.gamescope.as_ref() {
            gamescope.hide();
        } else {
            window.set_visible(false);
        }
    }

    pub fn toggle_window(&self) {
        if self.main_window_visible.get() {
            self.hide_window();
        } else {
            self.show_window();
        }
    }

    pub fn main_window_is_visible(&self) -> bool {
        self.main_window_visible.get()
    }

    pub fn register_controller_activation(
        &self,
        widget: &impl IsA<gtk4::Widget>,
        activate: impl Fn() + 'static,
    ) {
        self.controller_activations
            .borrow_mut()
            .push((widget.as_ref().downgrade(), Rc::new(activate)));
    }

    /// Handle normalized daemon controller input on the GTK main thread.
    pub fn handle_controller_action(self: &Rc<Self>, action: ControllerAction) {
        if !self.main_window_visible.get() {
            return;
        }
        let Some(gamescope) = self.gamescope.as_ref() else {
            return;
        };
        let Some(window) = gamescope.current_window() else {
            return;
        };
        window.set_focus_visible(true);
        match action {
            ControllerAction::Up => focus_direction(&window, gtk4::DirectionType::Up),
            ControllerAction::Down => focus_direction(&window, gtk4::DirectionType::Down),
            ControllerAction::Left => focus_direction(&window, gtk4::DirectionType::Left),
            ControllerAction::Right => focus_direction(&window, gtk4::DirectionType::Right),
            ControllerAction::Accept => match window_focus(&window) {
                None => {
                    window.child_focus(gtk4::DirectionType::TabForward);
                }
                Some(focused) => {
                    if !self.activate_controller_override(&focused) {
                        focused.activate();
                    }
                }
            },
            ControllerAction::Back => self.controller_back(&window),
        }
    }

    fn activate_controller_override(&self, focused: &gtk4::Widget) -> bool {
        let mut activations = self.controller_activations.borrow_mut();
        activations.retain(|(widget, _)| widget.upgrade().is_some());
        let activation = activations.iter().find_map(|(widget, activate)| {
            widget
                .upgrade()
                .filter(|widget| widget == focused)
                .map(|_| activate.clone())
        });
        drop(activations);
        if let Some(activate) = activation {
            activate();
            true
        } else {
            false
        }
    }

    fn controller_back(self: &Rc<Self>, window: &gtk4::Window) {
        if let Some(dialog) = window_focus(window).and_then(|focused| {
            focused
                .ancestor(adw::Dialog::static_type())
                .and_then(|ancestor| ancestor.downcast::<adw::Dialog>().ok())
        }) {
            dialog.close();
            return;
        }

        let is_main = self
            .main_window
            .borrow()
            .as_ref()
            .is_some_and(|main| main.upcast_ref::<gtk4::Window>() == window);
        if !is_main {
            window.close();
            return;
        }

        if let Some(stack) = find_named_descendant(window, "gamescope-main-pages")
            .and_then(|widget| widget.downcast::<gtk4::Stack>().ok())
            && stack.visible_child_name().as_deref() != Some("main")
        {
            stack.set_visible_child_name("main");
            return;
        }
        self.hide_window();
    }

    pub fn present_auxiliary(&self, window: &impl IsA<gtk4::Window>) {
        if let Some(gamescope) = self.gamescope.as_ref() {
            gamescope.present_auxiliary(window);
        } else {
            window.present();
        }
        self.flush_pending_toast();
    }

    pub fn present_existing_auxiliary(&self, window: &gtk4::Window) {
        if let Some(gamescope) = self.gamescope.as_ref() {
            gamescope.show_auxiliary(window);
        } else {
            window.present();
        }
        self.flush_pending_toast();
    }

    pub fn hide_auxiliary(&self, window: &gtk4::Window) {
        if let Some(gamescope) = self.gamescope.as_ref() {
            gamescope.hide_auxiliary(window);
        } else {
            window.set_visible(false);
        }
    }

    pub fn fans_window(&self) -> Option<gtk4::Window> {
        self.fans_window.borrow().clone()
    }

    pub fn set_fans_window(&self, window: &gtk4::Window) {
        *self.fans_window.borrow_mut() = Some(window.clone());
    }
}

fn focus_direction(window: &gtk4::Window, direction: gtk4::DirectionType) {
    if window_focus(window).is_none() {
        window.child_focus(gtk4::DirectionType::TabForward);
    } else {
        window.child_focus(direction);
    }
}

fn window_focus(window: &gtk4::Window) -> Option<gtk4::Widget> {
    gtk4::prelude::GtkWindowExt::focus(window)
}

fn find_named_descendant(root: &impl IsA<gtk4::Widget>, name: &str) -> Option<gtk4::Widget> {
    let root = root.as_ref();
    if root.widget_name() == name {
        return Some(root.clone());
    }
    let mut child = root.first_child();
    while let Some(widget) = child {
        if let Some(found) = find_named_descendant(&widget, name) {
            return Some(found);
        }
        child = widget.next_sibling();
    }
    None
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
