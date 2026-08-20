//! Gamescope X11 overlay lifecycle and resolution-aware UI scaling.
//!
//! Gamescope is deliberately treated as a separate presentation backend. Its
//! Xwayland surfaces remain mapped and are hidden with the EWMH opacity atom;
//! repeatedly unmapping a GTK toplevel can leave gamescope focusing an input
//! surface whose last buffer is no longer being composited.

use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::os::unix::fs::FileTypeExt;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use gtk::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, PropMode, Window};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const REFERENCE_WIDTH: f64 = 1707.0;
const MIN_SCALE: f64 = 1.0;
const MAX_SCALE: f64 = 3.0;
const FULL_OPACITY: u32 = u32::MAX;
const SCALE_ENV: &str = "Z13HELPER_GAMESCOPE_SCALE";

/// Force GTK onto gamescope's Xwayland server only when the advertised
/// gamescope Wayland socket is a real socket inside XDG_RUNTIME_DIR.
pub fn select_gdk_backend() {
    if validated_socket_from_env().is_some() && std::env::var_os("DISPLAY").is_some() {
        // SAFETY: this runs before GTK or any worker threads are initialized.
        unsafe { std::env::set_var("GDK_BACKEND", "x11") };
    }
}

fn validated_socket_from_env() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let display = std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY")?;
    let path = socket_path(Path::new(&runtime), &display)?;
    path.symlink_metadata()
        .ok()
        .filter(|metadata| metadata.file_type().is_socket())
        .map(|_| path)
}

fn socket_path(runtime: &Path, display: &OsStr) -> Option<PathBuf> {
    let display = Path::new(display);
    let mut components = display.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Some(runtime.join(display)),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct Atoms {
    overlay: Atom,
    external_overlay: Atom,
    input_focus: Atom,
    opacity: Atom,
}

/// Main-thread-owned gamescope presentation state.
pub struct Gamescope {
    conn: RustConnection,
    atoms: Atoms,
    scale: f64,
    output_width: i32,
    output_height: i32,
    windows: RefCell<Vec<glib::WeakRef<gtk::Window>>>,
    current: RefCell<Option<glib::WeakRef<gtk::Window>>>,
    visible: Cell<bool>,
}

impl Gamescope {
    pub fn connect() -> Option<Rc<Self>> {
        validated_socket_from_env()?;
        match Self::try_connect() {
            Ok(gamescope) => {
                tracing::info!(
                    scale = gamescope.scale,
                    width = gamescope.output_width,
                    height = gamescope.output_height,
                    "gamescope X11 overlay backend enabled"
                );
                Some(Rc::new(gamescope))
            }
            Err(error) => {
                tracing::error!(%error, "could not initialize gamescope X11 overlay backend");
                None
            }
        }
    }

    fn try_connect() -> Result<Self> {
        let (conn, screen_number) = x11rb::connect(None).context("connect to Xwayland")?;
        let screen = conn
            .setup()
            .roots
            .get(screen_number)
            .context("Xwayland did not advertise a screen")?;
        let fallback_dimensions = (
            i32::from(screen.width_in_pixels),
            i32::from(screen.height_in_pixels),
        );
        let (output_width, output_height) = gtk::gdk::Display::default()
            .and_then(|display| display.monitors().item(0))
            .and_downcast::<gtk::gdk::Monitor>()
            .map(|monitor| monitor.geometry())
            .map(|geometry| (geometry.width(), geometry.height()))
            .filter(|(width, height)| *width > 0 && *height > 0)
            .unwrap_or(fallback_dimensions);
        let scale_override = std::env::var(SCALE_ENV).unwrap_or_default();
        let scale = ui_scale(output_width, &scale_override);
        if !scale_override.is_empty() && parse_scale(&scale_override).is_none() {
            tracing::warn!(
                value = scale_override,
                "ignoring invalid gamescope UI scale override"
            );
        } else if parse_scale(&scale_override).is_some_and(|requested| requested != scale) {
            tracing::warn!(
                requested = scale_override,
                applied = scale,
                min = MIN_SCALE,
                max = MAX_SCALE,
                "clamped gamescope UI scale override"
            );
        }
        let atoms = Atoms {
            overlay: intern(&conn, b"STEAM_OVERLAY")?,
            external_overlay: intern(&conn, b"GAMESCOPE_EXTERNAL_OVERLAY")?,
            input_focus: intern(&conn, b"STEAM_INPUT_FOCUS")?,
            opacity: intern(&conn, b"_NET_WM_WINDOW_OPACITY")?,
        };
        Ok(Self {
            conn,
            atoms,
            scale,
            output_width,
            output_height,
            windows: RefCell::new(Vec::new()),
            current: RefCell::new(None),
            visible: Cell::new(false),
        })
    }

    pub fn scale(&self) -> f64 {
        self.scale
    }

    pub fn pixels(&self, logical: i32) -> i32 {
        (f64::from(logical) * self.scale).round() as i32
    }

    pub fn panel_size(&self, width: i32, height: i32) -> (i32, i32) {
        let horizontal_room = (f64::from(self.output_width) * 0.94).round() as i32;
        let vertical_room = (f64::from(self.output_height) * 0.90).round() as i32;
        (
            self.pixels(width).min(horizontal_room),
            self.pixels(height).min(vertical_room),
        )
    }

    pub fn wrap_panel(
        &self,
        child: &impl IsA<gtk::Widget>,
        width: i32,
        height: Option<i32>,
        on_backdrop: impl Fn() + 'static,
    ) -> gtk::Box {
        let wrapper = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        wrapper.add_css_class("gamescope-wrapper");

        let backdrop = gtk::Box::new(gtk::Orientation::Vertical, 0);
        backdrop.set_hexpand(true);
        backdrop.add_css_class("gamescope-backdrop");
        let click = gtk::GestureClick::new();
        click.connect_released(move |_, _, _, _| on_backdrop());
        backdrop.add_controller(click);

        let panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
        panel.add_css_class("gamescope-panel");
        let panel_width = self.pixels(width).min((self.output_width * 94) / 100);
        panel.set_width_request(panel_width);
        if let Some(height) = height {
            panel.set_height_request(self.panel_size(width, height).1);
        }
        panel.set_valign(gtk::Align::Center);
        panel.set_margin_top(self.output_height / 20);
        panel.set_margin_bottom(self.output_height / 20);
        let clamp = adw::Clamp::new();
        clamp.set_maximum_size(panel_width);
        clamp.set_tightening_threshold(panel_width);
        clamp.set_child(Some(child));
        panel.append(&clamp);

        wrapper.append(&backdrop);
        wrapper.append(&panel);
        wrapper
    }

    pub fn prepare_main(self: &Rc<Self>, window: &impl IsA<gtk::Window>, visible: bool) {
        let window = window.as_ref().clone();
        window.add_css_class("gamescope-ui");
        window.add_css_class("gamescope-overlay-window");
        window.set_decorated(false);
        window.set_resizable(true);
        window.fullscreen();
        self.visible.set(visible);
        self.register(&window);
        *self.current.borrow_mut() = Some(window.downgrade());
        // Map once and retain the surface for the life of the resident process.
        window.present();
        self.sync_windows();
    }

    pub fn present_auxiliary(self: &Rc<Self>, window: &impl IsA<gtk::Window>) {
        let window = window.as_ref().clone();
        window.add_css_class("gamescope-ui");
        self.register(&window);
        *self.current.borrow_mut() = Some(window.downgrade());

        let manager = self.clone();
        let closing = window.clone();
        window.connect_close_request(move |_| {
            manager.close_auxiliary(&closing);
            glib::Propagation::Proceed
        });

        window.present();
        self.sync_windows();
        let manager = self.clone();
        glib::idle_add_local_once(move || manager.sync_windows());
    }

    pub fn show_auxiliary(self: &Rc<Self>, window: &gtk::Window) {
        self.register(window);
        *self.current.borrow_mut() = Some(window.downgrade());
        window.present();
        self.sync_windows();
    }

    pub fn hide_auxiliary(&self, window: &gtk::Window) {
        window.set_visible(false);
        if self
            .current_window()
            .is_some_and(|current| current == *window)
        {
            *self.current.borrow_mut() = self
                .windows
                .borrow()
                .iter()
                .rev()
                .filter_map(glib::WeakRef::upgrade)
                .find(|candidate| candidate != window)
                .map(|candidate| candidate.downgrade());
        }
        self.sync_windows();
    }

    pub fn prepare_hud(self: &Rc<Self>, window: &impl IsA<gtk::Window>) {
        let window = window.as_ref().clone();
        window.add_css_class("gamescope-ui");
        window.set_default_size(self.pixels(300), self.pixels(100));
        let manager = self.clone();
        window.connect_realize(move |window| {
            let Some(xid) = xid(window) else {
                return;
            };
            if let Err(error) = manager.cardinal(xid, manager.atoms.external_overlay, 1) {
                tracing::warn!(%error, xid, "could not register gamescope HUD overlay");
            }
            let _ = manager.conn.flush();
        });
    }

    pub fn show(self: &Rc<Self>) {
        self.visible.set(true);
        if let Some(window) = self.current_window() {
            window.present();
        }
        self.sync_windows();
        let manager = self.clone();
        glib::idle_add_local_once(move || manager.sync_windows());
    }

    pub fn hide(&self) {
        self.visible.set(false);
        self.sync_windows();
    }

    fn register(self: &Rc<Self>, window: &gtk::Window) {
        self.windows
            .borrow_mut()
            .retain(|registered| registered.upgrade().is_some());
        if self
            .windows
            .borrow()
            .iter()
            .filter_map(glib::WeakRef::upgrade)
            .any(|registered| registered == *window)
        {
            return;
        }
        self.windows.borrow_mut().push(window.downgrade());
        let manager = self.clone();
        window.connect_realize(move |_| manager.sync_windows());
    }

    fn close_auxiliary(&self, closing: &gtk::Window) {
        self.windows.borrow_mut().retain(|registered| {
            registered
                .upgrade()
                .is_some_and(|window| window != *closing)
        });
        let closing_is_current = self
            .current_window()
            .is_some_and(|window| window == *closing);
        if closing_is_current {
            *self.current.borrow_mut() = self.windows.borrow().last().cloned();
        }
        if let Some(xid) = xid(closing) {
            let _ = self.set_window_state(xid, false);
            let _ = self.conn.flush();
        }
        self.sync_windows();
    }

    pub fn current_window(&self) -> Option<gtk::Window> {
        self.current
            .borrow()
            .as_ref()
            .and_then(glib::WeakRef::upgrade)
    }

    fn sync_windows(&self) {
        let current = self.current_window();
        let visible = self.visible.get();
        let mut windows = self.windows.borrow_mut();
        windows.retain(|registered| registered.upgrade().is_some());
        for window in windows.iter().filter_map(glib::WeakRef::upgrade) {
            let Some(xid) = xid(&window) else {
                continue;
            };
            let active = visible && current.as_ref().is_some_and(|active| *active == window);
            if let Err(error) = self.set_window_state(xid, active) {
                tracing::warn!(%error, xid, "could not update gamescope overlay window");
            }
        }
        if let Err(error) = self.conn.flush() {
            tracing::warn!(%error, "could not flush gamescope X11 state");
        }
    }

    fn set_window_state(&self, xid: Window, active: bool) -> Result<()> {
        self.cardinal(xid, self.atoms.overlay, 1)?;
        // Clear input first when hiding so an invisible surface can never retain
        // gamescope's input routing.
        if active {
            self.cardinal(xid, self.atoms.opacity, FULL_OPACITY)?;
            self.cardinal(xid, self.atoms.input_focus, 1)?;
        } else {
            self.cardinal(xid, self.atoms.input_focus, 0)?;
            self.cardinal(xid, self.atoms.opacity, 0)?;
        }
        Ok(())
    }

    fn cardinal(&self, xid: Window, atom: Atom, value: u32) -> Result<()> {
        self.conn
            .change_property32(PropMode::REPLACE, xid, atom, AtomEnum::CARDINAL, &[value])
            .context("change X11 overlay property")?;
        Ok(())
    }
}

fn xid(window: &gtk::Window) -> Option<Window> {
    window
        .surface()?
        .downcast::<gdk4_x11::X11Surface>()
        .ok()
        .map(|surface| surface.xid() as Window)
}

fn intern(conn: &RustConnection, name: &[u8]) -> Result<Atom> {
    Ok(conn
        .intern_atom(false, name)
        .context("intern gamescope X11 atom")?
        .reply()
        .context("read gamescope X11 atom reply")?
        .atom)
}

fn ui_scale(output_width: i32, scale_override: &str) -> f64 {
    parse_scale(scale_override)
        .unwrap_or_else(|| {
            if output_width <= 0 {
                MIN_SCALE
            } else {
                f64::from(output_width) / REFERENCE_WIDTH
            }
        })
        .clamp(MIN_SCALE, MAX_SCALE)
}

fn parse_scale(value: &str) -> Option<f64> {
    if value.is_empty() {
        return None;
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_name_stays_inside_runtime_directory() {
        let runtime = Path::new("/run/user/1000");
        assert_eq!(
            socket_path(runtime, OsStr::new("gamescope-0")),
            Some(runtime.join("gamescope-0"))
        );
        for invalid in ["", "../gamescope-0", "/tmp/gamescope-0", "a/b"] {
            assert_eq!(socket_path(runtime, OsStr::new(invalid)), None);
        }
    }

    #[test]
    fn scaling_matches_native_panel_and_clamps_overrides() {
        assert!((ui_scale(2560, "") - 2560.0 / REFERENCE_WIDTH).abs() < 0.0001);
        assert_eq!(ui_scale(1280, ""), MIN_SCALE);
        assert_eq!(ui_scale(7680, ""), MAX_SCALE);
        assert_eq!(ui_scale(2560, "1.25"), 1.25);
        assert_eq!(ui_scale(2560, "0.1"), MIN_SCALE);
        assert_eq!(ui_scale(2560, "30"), MAX_SCALE);
    }

    #[test]
    fn invalid_scaling_override_uses_auto_detection() {
        let automatic = ui_scale(2560, "");
        for invalid in ["garbage", "0", "-2", "NaN"] {
            assert_eq!(ui_scale(2560, invalid), automatic);
        }
    }
}
