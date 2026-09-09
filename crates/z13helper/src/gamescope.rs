//! Gamescope X11 overlay lifecycle and resolution-aware UI scaling.
//!
//! GTK owns the toplevels, but it never owns the X11 connection. The
//! connection and all X11 requests live on one resident owner thread. GTK
//! only submits a bounded, latest-state snapshot made from XIDs and scalar
//! desired state.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::FileTypeExt;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError, mpsc};
use std::thread::{self, JoinHandle};

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
const MAX_WINDOWS: usize = 32;
const X11_SOCKET_DIRECTORY: &str = "/tmp/.X11-unix";
const XWAYLAND_SERVER_ID_ATOM: &[u8] = b"GAMESCOPE_XWAYLAND_SERVER_ID";

#[derive(Debug, Eq, PartialEq)]
struct GamescopeDisplays {
    wayland: OsString,
    x11: OsString,
}

/// Force GTK onto gamescope's Xwayland server only after validating a real
/// gamescope Wayland socket inside XDG_RUNTIME_DIR.
pub fn select_gdk_backend() {
    let displays = resolve_gamescope_displays(
        std::env::var_os("XDG_RUNTIME_DIR")
            .as_deref()
            .map(Path::new),
        std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("DISPLAY").as_deref(),
        session_is_gamescope(),
        discover_gamescope_wayland,
        discover_primary_gamescope_x11,
    );
    if let Some(displays) = displays {
        // SAFETY: this runs before GTK or any worker threads are initialized.
        unsafe {
            std::env::set_var("GAMESCOPE_WAYLAND_DISPLAY", displays.wayland);
            std::env::set_var("DISPLAY", displays.x11);
            std::env::set_var("GDK_BACKEND", "x11");
        }
    }
}

fn resolve_gamescope_displays<F, G>(
    runtime: Option<&Path>,
    advertised_wayland: Option<&OsStr>,
    advertised_x11: Option<&OsStr>,
    session_is_gamescope: bool,
    discover_wayland: F,
    discover_x11: G,
) -> Option<GamescopeDisplays>
where
    F: FnOnce(&Path) -> Option<OsString>,
    G: FnOnce() -> Option<OsString>,
{
    let runtime = runtime?;
    let advertised_wayland = advertised_wayland
        .filter(|display| socket_path(runtime, display).is_some_and(|path| is_socket(&path)));
    let wayland_was_advertised = advertised_wayland.is_some();
    let wayland = match advertised_wayland {
        Some(display) => display.to_os_string(),
        None if session_is_gamescope => discover_wayland(runtime)?,
        None => return None,
    };
    let x11 = if wayland_was_advertised {
        advertised_x11
            .map(OsStr::to_os_string)
            .or_else(discover_x11)?
    } else {
        // A display inherited from an earlier desktop session may still be in
        // the user manager. Pair a discovered Wayland socket only with the
        // primary Xwayland server that identifies itself as Gamescope.
        discover_x11()?
    };
    Some(GamescopeDisplays { wayland, x11 })
}

fn session_is_gamescope() -> bool {
    ["XDG_CURRENT_DESKTOP", "XDG_SESSION_DESKTOP"]
        .into_iter()
        .filter_map(std::env::var_os)
        .any(|value| desktop_names_gamescope(&value))
}

fn desktop_names_gamescope(value: &OsStr) -> bool {
    value.to_str().is_some_and(|value| {
        value
            .split(':')
            .any(|desktop| desktop.eq_ignore_ascii_case("gamescope"))
    })
}

fn discover_gamescope_wayland(runtime: &Path) -> Option<OsString> {
    let displays = runtime
        .read_dir()
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            gamescope_wayland_name(&name)
                .then(|| entry.path())
                .filter(|path| is_socket(path))
                .map(|_| name)
        });
    exactly_one(displays)
}

fn gamescope_wayland_name(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name.strip_prefix("gamescope-").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
    })
}

fn discover_primary_gamescope_x11() -> Option<OsString> {
    let displays = Path::new(X11_SOCKET_DIRECTORY)
        .read_dir()
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            is_socket(&entry.path())
                .then(|| x11_display_from_socket_name(&entry.file_name()))
                .flatten()
        });
    select_primary_x11_display(displays, is_primary_gamescope_x11)
}

fn x11_display_from_socket_name(name: &OsStr) -> Option<OsString> {
    let suffix = name.to_str()?.strip_prefix('X')?;
    (!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| OsString::from(format!(":{suffix}")))
}

fn select_primary_x11_display<I, F>(displays: I, mut is_primary: F) -> Option<OsString>
where
    I: IntoIterator<Item = OsString>,
    F: FnMut(&OsStr) -> bool,
{
    exactly_one(displays.into_iter().filter(|display| is_primary(display)))
}

fn is_primary_gamescope_x11(display: &OsStr) -> bool {
    display.to_str().and_then(gamescope_xwayland_server_id) == Some(0)
}

fn gamescope_xwayland_server_id(display: &str) -> Option<u32> {
    let (conn, screen_number) = x11rb::connect(Some(display)).ok()?;
    let root = conn.setup().roots.get(screen_number)?.root;
    let atom = conn
        .intern_atom(true, XWAYLAND_SERVER_ID_ATOM)
        .ok()?
        .reply()
        .ok()?
        .atom;
    if atom == u32::from(AtomEnum::NONE) {
        return None;
    }
    let property = conn
        .get_property(false, root, atom, AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    (property.format == 32 && property.type_ == u32::from(AtomEnum::CARDINAL))
        .then(|| property.value32()?.next())
        .flatten()
}

fn exactly_one<I>(values: I) -> Option<I::Item>
where
    I: IntoIterator,
{
    let mut values = values.into_iter();
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn is_socket(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_socket())
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowState {
    xid: Window,
    /// Interactive windows use this for active/current. HUD windows use it
    /// for visible external-overlay presentation and never receive focus.
    active: bool,
    /// GTK keeps Gamescope windows mapped; hidden state is represented by
    /// opacity and input properties instead of unmapping the surface.
    mapped: bool,
    external_overlay: bool,
}

#[derive(Default)]
struct StateMailbox {
    latest: Mutex<Option<Vec<WindowState>>>,
    last_submitted: Mutex<HashMap<Window, WindowState>>,
    forced_hidden: Mutex<HashSet<Window>>,
    wake: Mutex<Option<mpsc::SyncSender<()>>>,
    closed: AtomicBool,
}

impl StateMailbox {
    fn submit(&self, states: &[WindowState]) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        let states = states.iter().copied().take(MAX_WINDOWS).collect::<Vec<_>>();
        let next = states
            .iter()
            .copied()
            .map(|state| (state.xid, state))
            .collect::<HashMap<_, _>>();
        let Ok(mut latest) = self.latest.lock() else {
            return false;
        };
        let Ok(mut last_submitted) = self.last_submitted.lock() else {
            return false;
        };
        let Ok(mut forced_hidden) = self.forced_hidden.lock() else {
            return false;
        };
        for (xid, old) in last_submitted.iter() {
            let remains_visible = next.get(xid).is_some_and(|new| new.active && new.mapped);
            if old.active && old.mapped && !remains_visible {
                // Preserve the safety edge even when a newer snapshot replaces
                // the hidden snapshot before the owner wakes up.
                forced_hidden.insert(*xid);
            }
        }
        *last_submitted = next;
        *latest = Some(states);
        drop(latest);
        let Ok(wake) = self.wake.lock() else {
            return false;
        };
        let Some(wake) = wake.as_ref() else {
            return false;
        };
        match wake.try_send(()) {
            Ok(()) | Err(mpsc::TrySendError::Full(())) => true,
            Err(mpsc::TrySendError::Disconnected(())) => false,
        }
    }

    fn take(&self) -> Option<PendingSnapshot> {
        let states = self.latest.lock().ok()?.take();
        let forced_hidden: HashSet<Window> = self.forced_hidden.lock().ok()?.drain().collect();
        if states.is_none() && forced_hidden.is_empty() {
            return None;
        }
        Some(PendingSnapshot {
            states: states.unwrap_or_default(),
            forced_hidden,
        })
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        if let Ok(wake) = self.wake.lock()
            && let Some(wake) = wake.as_ref()
        {
            let _ = wake.try_send(());
        }
    }
}

struct PendingSnapshot {
    states: Vec<WindowState>,
    forced_hidden: HashSet<Window>,
}

struct X11Owner {
    mailbox: Arc<StateMailbox>,
    thread: Option<JoinHandle<()>>,
}

impl X11Owner {
    fn start() -> Result<(Self, (i32, i32))> {
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let mailbox = Arc::new(StateMailbox::default());
        *mailbox.wake.lock().expect("new gamescope mailbox") = Some(wake_tx);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread_mailbox = Arc::clone(&mailbox);
        let thread = thread::Builder::new()
            .name("z13helper-gamescope-x11".into())
            .spawn(move || match X11Backend::connect() {
                Ok((backend, dimensions)) => {
                    let _ = ready_tx.send(Ok(dimensions));
                    owner_loop(backend, thread_mailbox, wake_rx);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                }
            })
            .context("start gamescope X11 owner")?;
        let dimensions = match ready_rx.recv() {
            Ok(Ok(dimensions)) => dimensions,
            Ok(Err(error)) => {
                mailbox.close();
                let _ = thread.join();
                return Err(anyhow::anyhow!(error));
            }
            Err(error) => {
                mailbox.close();
                let _ = thread.join();
                return Err(anyhow::anyhow!(
                    "gamescope owner stopped during startup: {error}"
                ));
            }
        };
        Ok((
            Self {
                mailbox,
                thread: Some(thread),
            },
            dimensions,
        ))
    }

    fn submit(&self, states: &[WindowState]) {
        if !self.mailbox.submit(states) {
            tracing::debug!("gamescope state update was coalesced or owner is stopping");
        }
    }
}

impl Drop for X11Owner {
    fn drop(&mut self) {
        self.mailbox.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

trait X11Io {
    fn set_overlay(&mut self, xid: Window, external: bool) -> Result<()>;
    fn set_input_focus(&mut self, xid: Window, focused: bool) -> Result<()>;
    fn set_opacity(&mut self, xid: Window, opacity: u32) -> Result<()>;
    fn flush(&mut self) -> Result<()>;
}

struct X11Backend {
    conn: RustConnection,
    atoms: Atoms,
}

impl X11Backend {
    fn connect() -> Result<(Self, (i32, i32))> {
        let (conn, screen_number) = x11rb::connect(None).context("connect to Xwayland")?;
        let screen = conn
            .setup()
            .roots
            .get(screen_number)
            .context("Xwayland did not advertise a screen")?;
        let dimensions = (
            i32::from(screen.width_in_pixels),
            i32::from(screen.height_in_pixels),
        );
        let atoms = Atoms {
            overlay: intern(&conn, b"STEAM_OVERLAY")?,
            external_overlay: intern(&conn, b"GAMESCOPE_EXTERNAL_OVERLAY")?,
            input_focus: intern(&conn, b"STEAM_INPUT_FOCUS")?,
            opacity: intern(&conn, b"_NET_WM_WINDOW_OPACITY")?,
        };
        Ok((Self { conn, atoms }, dimensions))
    }

    fn cardinal(&mut self, xid: Window, atom: Atom, value: u32) -> Result<()> {
        self.conn
            .change_property32(PropMode::REPLACE, xid, atom, AtomEnum::CARDINAL, &[value])
            .context("change X11 overlay property")?;
        Ok(())
    }
}

impl X11Io for X11Backend {
    fn set_overlay(&mut self, xid: Window, external: bool) -> Result<()> {
        self.cardinal(xid, self.atoms.overlay, 1)?;
        if external {
            self.cardinal(xid, self.atoms.external_overlay, 1)?;
        }
        Ok(())
    }

    fn set_input_focus(&mut self, xid: Window, focused: bool) -> Result<()> {
        self.cardinal(xid, self.atoms.input_focus, u32::from(focused))
    }

    fn set_opacity(&mut self, xid: Window, opacity: u32) -> Result<()> {
        self.cardinal(xid, self.atoms.opacity, opacity)
    }

    fn flush(&mut self) -> Result<()> {
        self.conn.flush().context("flush gamescope X11 state")
    }
}

fn owner_loop<S: X11Io>(mut service: S, mailbox: Arc<StateMailbox>, wake_rx: mpsc::Receiver<()>) {
    let mut applied = HashMap::<Window, WindowState>::new();
    while wake_rx.recv().is_ok() {
        if mailbox.closed.load(Ordering::Acquire) {
            return;
        }
        while let Some(snapshot) = mailbox.take() {
            apply_snapshot(
                &mut service,
                &mut applied,
                &snapshot.states,
                &snapshot.forced_hidden,
            );
            if mailbox.closed.load(Ordering::Acquire) || !has_pending_state(&mailbox) {
                break;
            }
        }
    }
}

fn has_pending_state(mailbox: &StateMailbox) -> bool {
    let latest_pending = match mailbox.latest.try_lock() {
        Ok(latest) => latest.is_some(),
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Poisoned(_)) => false,
    };
    if latest_pending {
        return true;
    }
    match mailbox.forced_hidden.try_lock() {
        Ok(forced_hidden) => !forced_hidden.is_empty(),
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Poisoned(_)) => false,
    }
}

fn apply_snapshot<S: X11Io>(
    service: &mut S,
    applied: &mut HashMap<Window, WindowState>,
    states: &[WindowState],
    forced_hidden: &HashSet<Window>,
) {
    let next = states
        .iter()
        .copied()
        .take(MAX_WINDOWS)
        .map(|state| (state.xid, state))
        .collect::<HashMap<_, _>>();

    // A hidden edge is retained separately from the latest snapshot. This
    // makes focus-before-opacity ordering observable even for a hide/show
    // burst that is coalesced into one latest snapshot.
    let mut forced = forced_hidden.iter().copied().collect::<Vec<_>>();
    forced.sort_unstable();
    for xid in forced {
        let prior = applied
            .get(&xid)
            .copied()
            .or_else(|| next.get(&xid).copied())
            .unwrap_or(WindowState {
                xid,
                active: false,
                mapped: true,
                external_overlay: false,
            });
        let hidden = WindowState {
            active: false,
            mapped: true,
            ..prior
        };
        apply_one(service, hidden);
        if next.contains_key(&xid) {
            applied.insert(xid, hidden);
        } else {
            applied.remove(&xid);
        }
    }

    // Deactivate old windows first. This makes focus-before-opacity ordering
    // explicit for hidden surfaces and prevents two active toplevels.
    let mut old_states = applied.values().copied().collect::<Vec<_>>();
    old_states.sort_unstable_by_key(|state| state.xid);
    for old in old_states {
        if !next.contains_key(&old.xid) {
            apply_one(
                service,
                WindowState {
                    active: false,
                    ..old
                },
            );
        }
    }
    let mut next_states = next.values().copied().collect::<Vec<_>>();
    next_states.sort_unstable_by_key(|state| state.xid);
    for state in next_states {
        if applied.get(&state.xid) != Some(&state) {
            apply_one(service, state);
        }
    }
    if let Err(error) = service.flush() {
        tracing::debug!(%error, "could not flush gamescope X11 state");
    }
    *applied = next;
}

fn apply_one<S: X11Io>(service: &mut S, state: WindowState) {
    if let Err(error) = service.set_overlay(state.xid, state.external_overlay) {
        tracing::debug!(%error, xid = state.xid, "could not set gamescope overlay property");
    }
    let visible = state.active && state.mapped;
    if !visible {
        // Critical ordering: invisible windows lose Gamescope input before
        // their opacity is cleared.
        if let Err(error) = service.set_input_focus(state.xid, false) {
            tracing::debug!(%error, xid = state.xid, "could not clear gamescope input focus");
        }
        if let Err(error) = service.set_opacity(state.xid, 0) {
            tracing::debug!(%error, xid = state.xid, "could not hide gamescope window");
        }
    } else if state.external_overlay {
        let _ = service.set_input_focus(state.xid, false);
        let _ = service.set_opacity(state.xid, FULL_OPACITY);
    } else {
        let _ = service.set_opacity(state.xid, FULL_OPACITY);
        let _ = service.set_input_focus(state.xid, true);
    }
}

// Main-thread-owned Gamescope presentation state. It contains GTK weak refs,
// but the X11 owner above receives only WindowState values.
pub struct Gamescope {
    owner: X11Owner,
    scale: f64,
    output_width: i32,
    output_height: i32,
    windows: RefCell<Vec<RegisteredWindow>>,
    hud_windows: RefCell<Vec<glib::WeakRef<gtk::Window>>>,
    current: RefCell<Option<glib::WeakRef<gtk::Window>>>,
    visible: Cell<bool>,
}

struct RegisteredWindow {
    window: glib::WeakRef<gtk::Window>,
    hidden: bool,
}

impl Gamescope {
    pub fn connect() -> Option<Rc<Self>> {
        validated_socket_from_env()?;
        match X11Owner::start() {
            Ok((owner, (output_width, output_height))) => {
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
                tracing::info!(
                    scale,
                    output_width,
                    output_height,
                    "gamescope X11 overlay backend enabled"
                );
                Some(Rc::new(Self {
                    owner,
                    scale,
                    output_width,
                    output_height,
                    windows: RefCell::new(Vec::new()),
                    hud_windows: RefCell::new(Vec::new()),
                    current: RefCell::new(None),
                    visible: Cell::new(false),
                }))
            }
            Err(error) => {
                tracing::error!(%error, "could not initialize gamescope X11 overlay backend");
                None
            }
        }
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
    ) -> gtk::Overlay {
        let wrapper = gtk::Overlay::new();
        wrapper.add_css_class("gamescope-wrapper");
        let backdrop = gtk::Box::new(gtk::Orientation::Vertical, 0);
        backdrop.add_css_class("gamescope-backdrop");
        let click = gtk::GestureClick::new();
        click.connect_released(move |_, _, _, _| on_backdrop());
        backdrop.add_controller(click);
        wrapper.set_child(Some(&backdrop));

        let panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
        panel.add_css_class("gamescope-panel");
        let panel_width = self.pixels(width).min((self.output_width * 94) / 100);
        panel.set_width_request(panel_width);
        if let Some(height) = height {
            panel.set_height_request(self.panel_size(width, height).1);
        }
        panel.set_halign(gtk::Align::Center);
        panel.set_valign(gtk::Align::Center);
        panel.set_margin_top(self.output_height / 20);
        panel.set_margin_bottom(self.output_height / 20);
        let clamp = adw::Clamp::new();
        clamp.set_maximum_size(panel_width);
        clamp.set_tightening_threshold(panel_width);
        clamp.set_child(Some(child));
        panel.append(&clamp);
        wrapper.add_overlay(&panel);
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
        self.register(&window, false);
        *self.current.borrow_mut() = Some(window.downgrade());
        window.present();
        self.sync_windows();
    }

    pub fn present_auxiliary(self: &Rc<Self>, window: &impl IsA<gtk::Window>) {
        let window = window.as_ref().clone();
        window.add_css_class("gamescope-ui");
        self.register(&window, false);
        *self.current.borrow_mut() = Some(window.downgrade());
        let manager = self.clone();
        let closing = window.clone();
        window.connect_close_request(move |_| {
            manager.close_auxiliary(&closing);
            glib::Propagation::Proceed
        });
        window.present();
        self.sync_windows();
    }

    pub fn show_auxiliary(self: &Rc<Self>, window: &gtk::Window) {
        self.register(window, false);
        *self.current.borrow_mut() = Some(window.downgrade());
        window.present();
        self.sync_windows();
    }

    pub fn hide_auxiliary(&self, window: &gtk::Window) {
        // Keep the Xwayland surface mapped. Hide through the owner-thread
        // state instead of GTK unmapping the surface.
        window.present();
        if let Some(entry) = self
            .windows
            .borrow_mut()
            .iter_mut()
            .find(|entry| entry.window.upgrade().as_ref() == Some(window))
        {
            entry.hidden = true;
        }
        if self
            .current_window()
            .is_some_and(|current| current == *window)
        {
            *self.current.borrow_mut() = self
                .windows
                .borrow()
                .iter()
                .rev()
                .find_map(|entry| (!entry.hidden).then(|| entry.window.upgrade()).flatten())
                .map(|candidate| candidate.downgrade());
        }
        self.sync_windows();
    }

    pub fn prepare_hud(self: &Rc<Self>, window: &impl IsA<gtk::Window>) {
        let window = window.as_ref().clone();
        window.add_css_class("gamescope-ui");
        window.set_default_size(self.pixels(300), self.pixels(100));
        let manager = self.clone();
        window.connect_realize(move |window| manager.register_hud(window));
        let manager = self.clone();
        let closing = window.clone();
        window.connect_close_request(move |_| {
            manager.hud_windows.borrow_mut().retain(|registered| {
                registered
                    .upgrade()
                    .is_some_and(|candidate| candidate != closing)
            });
            manager.sync_windows();
            glib::Propagation::Proceed
        });
    }

    pub fn show(self: &Rc<Self>) {
        self.visible.set(true);
        if let Some(window) = self.current_window() {
            window.present();
        }
        self.sync_windows();
    }

    pub fn hide(&self) {
        self.visible.set(false);
        self.sync_windows();
    }

    fn register(self: &Rc<Self>, window: &gtk::Window, hidden: bool) {
        self.windows
            .borrow_mut()
            .retain(|entry| entry.window.upgrade().is_some());
        if let Some(entry) = self.windows.borrow_mut().iter_mut().find(|entry| {
            entry
                .window
                .upgrade()
                .is_some_and(|registered| registered == *window)
        }) {
            entry.hidden = hidden;
            return;
        }
        self.windows.borrow_mut().push(RegisteredWindow {
            window: window.downgrade(),
            hidden,
        });
        let manager = self.clone();
        window.connect_realize(move |_| manager.sync_windows());
    }

    fn register_hud(&self, window: &gtk::Window) {
        self.hud_windows
            .borrow_mut()
            .retain(|registered| registered.upgrade().is_some());
        if !self.hud_windows.borrow().iter().any(|registered| {
            registered
                .upgrade()
                .is_some_and(|candidate| candidate == *window)
        }) {
            self.hud_windows.borrow_mut().push(window.downgrade());
        }
        self.sync_windows();
    }

    fn close_auxiliary(&self, closing: &gtk::Window) {
        self.windows.borrow_mut().retain(|entry| {
            entry
                .window
                .upgrade()
                .is_some_and(|window| window != *closing)
        });
        if self
            .current_window()
            .is_some_and(|window| window == *closing)
        {
            *self.current.borrow_mut() = self
                .windows
                .borrow()
                .iter()
                .rev()
                .find_map(|entry| (!entry.hidden).then(|| entry.window.upgrade()).flatten())
                .map(|candidate| candidate.downgrade());
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
        let mut states = Vec::with_capacity(MAX_WINDOWS);
        let mut windows = self.windows.borrow_mut();
        windows.retain(|entry| entry.window.upgrade().is_some());
        for entry in windows.iter() {
            let Some(window) = entry.window.upgrade() else {
                continue;
            };
            let Some(xid) = xid(&window) else {
                continue;
            };
            let active = visible
                && !entry.hidden
                && current.as_ref().is_some_and(|active| *active == window);
            states.push(WindowState {
                xid,
                active,
                mapped: true,
                external_overlay: false,
            });
        }
        drop(windows);
        let mut hud_windows = self.hud_windows.borrow_mut();
        hud_windows.retain(|registered| registered.upgrade().is_some());
        for registered in hud_windows.iter() {
            let Some(window) = registered.upgrade() else {
                continue;
            };
            let Some(xid) = xid(&window) else {
                continue;
            };
            states.push(WindowState {
                xid,
                active: true,
                mapped: true,
                external_overlay: true,
            });
        }
        self.owner.submit(&states);
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
    use std::fs;
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    struct FakeX11 {
        operations: Arc<Mutex<Vec<String>>>,
        flushes: Arc<Mutex<usize>>,
        flush_events: mpsc::Sender<usize>,
        first_flush_release: Option<mpsc::Receiver<()>>,
    }

    impl X11Io for FakeX11 {
        fn set_overlay(&mut self, xid: Window, external: bool) -> Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(format!("overlay:{xid}:{external}"));
            Ok(())
        }
        fn set_input_focus(&mut self, xid: Window, focused: bool) -> Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(format!("focus:{xid}:{focused}"));
            Ok(())
        }
        fn set_opacity(&mut self, xid: Window, opacity: u32) -> Result<()> {
            self.operations
                .lock()
                .unwrap()
                .push(format!("opacity:{xid}:{opacity}"));
            Ok(())
        }
        fn flush(&mut self) -> Result<()> {
            let mut flushes = self.flushes.lock().unwrap();
            *flushes += 1;
            let count = *flushes;
            drop(flushes);
            let _ = self.flush_events.send(count);
            if count == 1
                && let Some(release) = self.first_flush_release.take()
            {
                release.recv().unwrap();
            }
            Ok(())
        }
    }

    fn fake_owner(service: FakeX11) -> (Arc<StateMailbox>, JoinHandle<()>) {
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let mailbox = Arc::new(StateMailbox::default());
        *mailbox.wake.lock().unwrap() = Some(wake_tx);
        let thread_mailbox = Arc::clone(&mailbox);
        let thread = thread::spawn(move || owner_loop(service, thread_mailbox, wake_rx));
        (mailbox, thread)
    }

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
    fn gamescope_session_discovers_displays_when_environment_file_is_absent() {
        let runtime = std::env::temp_dir().join(format!(
            "z13helper-gamescope-display-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&runtime);
        fs::create_dir(&runtime).unwrap();
        let listener = UnixListener::bind(runtime.join("gamescope-0")).unwrap();

        let displays = resolve_gamescope_displays(
            Some(&runtime),
            None,
            None,
            true,
            discover_gamescope_wayland,
            || Some(OsString::from(":0")),
        );

        assert_eq!(
            displays,
            Some(GamescopeDisplays {
                wayland: OsString::from("gamescope-0"),
                x11: OsString::from(":0"),
            })
        );
        drop(listener);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn ordinary_desktop_does_not_adopt_discovered_gamescope_displays() {
        let displays = resolve_gamescope_displays(
            Some(Path::new("/run/user/1000")),
            None,
            Some(OsStr::new(":2")),
            false,
            |_| Some(OsString::from("gamescope-0")),
            || Some(OsString::from(":0")),
        );
        assert_eq!(displays, None);
    }

    #[test]
    fn gamescope_desktop_marker_accepts_colon_separated_names() {
        assert!(desktop_names_gamescope(OsStr::new("gamescope")));
        assert!(desktop_names_gamescope(OsStr::new("KDE:gamescope")));
        assert!(!desktop_names_gamescope(OsStr::new("KDE")));
        assert!(!desktop_names_gamescope(OsStr::new("gamescope-session")));
    }

    #[test]
    fn primary_xwayland_discovery_requires_one_gamescope_server_zero() {
        let displays = vec![OsString::from(":0"), OsString::from(":1")];
        assert_eq!(
            select_primary_x11_display(displays.clone(), |display| display == ":0"),
            Some(OsString::from(":0"))
        );
        assert_eq!(select_primary_x11_display(displays.clone(), |_| true), None);
        assert_eq!(select_primary_x11_display(displays, |_| false), None);
    }

    #[test]
    fn x11_socket_names_are_restricted_to_numeric_displays() {
        assert_eq!(
            x11_display_from_socket_name(OsStr::new("X12")),
            Some(OsString::from(":12"))
        );
        for invalid in ["X", "Xfoo", "X1-lock", "gamescope-0"] {
            assert_eq!(x11_display_from_socket_name(OsStr::new(invalid)), None);
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
        let automatic = ui_scale(2560, "");
        for invalid in ["garbage", "0", "-2", "NaN"] {
            assert_eq!(ui_scale(2560, invalid), automatic);
        }
    }

    #[test]
    fn gtk_facing_mailbox_submit_does_not_run_x11_io_and_coalesces() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let flushes = Arc::new(Mutex::new(0));
        let (flush_events_tx, flush_events_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let service = FakeX11 {
            operations: Arc::clone(&operations),
            flushes: Arc::clone(&flushes),
            flush_events: flush_events_tx,
            first_flush_release: Some(release_rx),
        };
        let (mailbox, thread) = fake_owner(service);
        assert!(mailbox.submit(&[WindowState {
            xid: 7,
            active: true,
            mapped: true,
            external_overlay: false
        }]));
        assert_eq!(flush_events_rx.recv_timeout(Duration::from_secs(1)), Ok(1));
        assert!(mailbox.submit(&[WindowState {
            xid: 9,
            active: false,
            mapped: true,
            external_overlay: false
        }]));
        release_tx.send(()).unwrap();
        assert_eq!(flush_events_rx.recv_timeout(Duration::from_secs(1)), Ok(2));
        mailbox.close();
        thread.join().unwrap();
        let operations = operations.lock().unwrap();
        assert!(operations.starts_with(&[
            "overlay:7:false".to_string(),
            "opacity:7:4294967295".to_string(),
            "focus:7:true".to_string()
        ]));
        assert!(operations.ends_with(&[
            "overlay:9:false".to_string(),
            "focus:9:false".to_string(),
            "opacity:9:0".to_string()
        ]));
        assert_eq!(*flushes.lock().unwrap(), 2);
    }

    #[test]
    fn hidden_window_clears_input_before_opacity_and_stays_mapped() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let flushes = Arc::new(Mutex::new(0));
        let (flush_events_tx, flush_events_rx) = mpsc::channel();
        let service = FakeX11 {
            operations: Arc::clone(&operations),
            flushes: Arc::clone(&flushes),
            flush_events: flush_events_tx,
            first_flush_release: None,
        };
        let (mailbox, thread) = fake_owner(service);
        mailbox.submit(&[WindowState {
            xid: 11,
            active: true,
            mapped: true,
            external_overlay: false,
        }]);
        assert_eq!(flush_events_rx.recv_timeout(Duration::from_secs(1)), Ok(1));
        operations.lock().unwrap().clear();
        mailbox.submit(&[WindowState {
            xid: 11,
            active: false,
            mapped: true,
            external_overlay: false,
        }]);
        assert_eq!(flush_events_rx.recv_timeout(Duration::from_secs(1)), Ok(2));
        mailbox.close();
        thread.join().unwrap();
        assert_eq!(
            operations.lock().unwrap().as_slice(),
            &["overlay:11:false", "focus:11:false", "opacity:11:0"]
        );
        assert_eq!(*flushes.lock().unwrap(), 2);
    }

    #[test]
    fn coalesced_hide_show_still_clears_focus_before_showing_again() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let flushes = Arc::new(Mutex::new(0));
        let (flush_events_tx, flush_events_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let service = FakeX11 {
            operations: Arc::clone(&operations),
            flushes: Arc::clone(&flushes),
            flush_events: flush_events_tx,
            first_flush_release: Some(release_rx),
        };
        let (mailbox, thread) = fake_owner(service);
        let active = WindowState {
            xid: 21,
            active: true,
            mapped: true,
            external_overlay: false,
        };
        mailbox.submit(&[active]);
        assert_eq!(flush_events_rx.recv_timeout(Duration::from_secs(1)), Ok(1));
        mailbox.submit(&[WindowState {
            active: false,
            ..active
        }]);
        mailbox.submit(&[active]);
        release_tx.send(()).unwrap();
        assert_eq!(flush_events_rx.recv_timeout(Duration::from_secs(1)), Ok(2));
        mailbox.close();
        thread.join().unwrap();

        let operations = operations.lock().unwrap();
        assert_eq!(
            &operations[3..6],
            ["overlay:21:false", "focus:21:false", "opacity:21:0"]
        );
        assert_eq!(
            &operations[6..9],
            ["overlay:21:false", "opacity:21:4294967295", "focus:21:true"]
        );
    }

    #[test]
    fn brand_new_inactive_window_is_explicitly_hidden_and_mapped() {
        let operations = Arc::new(Mutex::new(Vec::new()));
        let flushes = Arc::new(Mutex::new(0));
        let (flush_events_tx, flush_events_rx) = mpsc::channel();
        let service = FakeX11 {
            operations: Arc::clone(&operations),
            flushes: Arc::clone(&flushes),
            flush_events: flush_events_tx,
            first_flush_release: None,
        };
        let (mailbox, thread) = fake_owner(service);
        let hidden = WindowState {
            xid: 31,
            active: false,
            mapped: true,
            external_overlay: false,
        };
        mailbox.submit(&[hidden]);
        assert_eq!(flush_events_rx.recv_timeout(Duration::from_secs(1)), Ok(1));
        mailbox.close();
        thread.join().unwrap();
        assert_eq!(
            operations.lock().unwrap().as_slice(),
            ["overlay:31:false", "focus:31:false", "opacity:31:0"]
        );
        assert!(hidden.mapped);
        assert!(!hidden.active);
    }
}
