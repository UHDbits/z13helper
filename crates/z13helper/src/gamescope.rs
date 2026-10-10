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
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
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

/// Set once `select_gdk_backend` validated gamescope's displays; overlay mode
/// follows that single decision instead of re-reading the environment.
static GAMESCOPE_SELECTED: AtomicBool = AtomicBool::new(false);

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
        is_primary_gamescope_x11,
    );
    if let Some(displays) = displays {
        // SAFETY: this runs before GTK or any worker threads are initialized.
        unsafe {
            std::env::set_var("GAMESCOPE_WAYLAND_DISPLAY", displays.wayland);
            std::env::set_var("DISPLAY", displays.x11);
            std::env::set_var("GDK_BACKEND", "x11");
        }
        GAMESCOPE_SELECTED.store(true, Ordering::Relaxed);
    }
}

fn resolve_gamescope_displays<F, G, H>(
    runtime: Option<&Path>,
    advertised_wayland: Option<&OsStr>,
    advertised_x11: Option<&OsStr>,
    session_is_gamescope: bool,
    discover_wayland: F,
    discover_x11: G,
    is_primary_x11: H,
) -> Option<GamescopeDisplays>
where
    F: FnOnce(&Path) -> Option<OsString>,
    G: FnOnce() -> Option<OsString>,
    H: FnOnce(&OsStr) -> bool,
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
        // The advertised DISPLAY may also be stale desktop state.
        advertised_x11
            .filter(|display| is_primary_x11(display))
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

/// A non-symlink socket that accepts connections; a crashed server leaves a
/// socket file behind that refuses them.
fn is_socket(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_socket())
        && UnixStream::connect(path).is_ok()
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

/// GTK keeps Gamescope windows mapped; hidden state is represented by opacity
/// and input properties instead of unmapping the surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowState {
    xid: Window,
    /// Interactive windows use this for active/current. HUD windows use it
    /// for visible external-overlay presentation and never receive focus.
    active: bool,
    external_overlay: bool,
}

#[derive(Default)]
struct MailboxState {
    latest: Option<Vec<WindowState>>,
    last_submitted: HashMap<Window, WindowState>,
    forced_hidden: HashSet<Window>,
    closed: bool,
}

#[derive(Default)]
struct StateMailbox {
    state: Mutex<MailboxState>,
    changed: Condvar,
}

impl StateMailbox {
    fn submit(&self, states: &[WindowState]) -> bool {
        let Ok(mut guard) = self.state.lock() else {
            return false;
        };
        let mailbox = &mut *guard;
        if mailbox.closed {
            return false;
        }
        let states = states.iter().copied().take(MAX_WINDOWS).collect::<Vec<_>>();
        let next = states
            .iter()
            .copied()
            .map(|state| (state.xid, state))
            .collect::<HashMap<_, _>>();
        for (xid, old) in &mailbox.last_submitted {
            if old.active && !next.get(xid).is_some_and(|new| new.active) {
                // Preserve the safety edge even when a newer snapshot replaces
                // the hidden snapshot before the owner wakes up.
                mailbox.forced_hidden.insert(*xid);
            }
        }
        mailbox.last_submitted = next;
        mailbox.latest = Some(states);
        self.changed.notify_one();
        true
    }

    /// Blocks until a snapshot or hidden edge is pending; `None` once closed.
    fn wait(&self) -> Option<PendingSnapshot> {
        let mut mailbox = self
            .changed
            .wait_while(self.state.lock().ok()?, |mailbox| {
                mailbox.latest.is_none() && mailbox.forced_hidden.is_empty() && !mailbox.closed
            })
            .ok()?;
        if mailbox.closed {
            return None;
        }
        Some(PendingSnapshot {
            states: mailbox.latest.take().unwrap_or_default(),
            forced_hidden: std::mem::take(&mut mailbox.forced_hidden),
        })
    }

    fn close(&self) {
        if let Ok(mut mailbox) = self.state.lock() {
            mailbox.closed = true;
        }
        self.changed.notify_one();
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
        let mailbox = Arc::new(StateMailbox::default());
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread_mailbox = Arc::clone(&mailbox);
        let thread = thread::Builder::new()
            .name("z13helper-gamescope-x11".into())
            .spawn(move || match X11Backend::connect() {
                Ok((backend, dimensions)) => {
                    let _ = ready_tx.send(Ok(dimensions));
                    owner_loop(backend, &thread_mailbox);
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
            tracing::debug!("gamescope state update dropped: owner is stopping");
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

fn owner_loop<S: X11Io>(mut service: S, mailbox: &StateMailbox) {
    let mut applied = HashMap::<Window, WindowState>::new();
    while let Some(snapshot) = mailbox.wait() {
        apply_snapshot(
            &mut service,
            &mut applied,
            &snapshot.states,
            &snapshot.forced_hidden,
        );
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
                external_overlay: false,
            });
        let hidden = WindowState {
            active: false,
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
    if !state.active {
        // Critical ordering: invisible windows lose Gamescope input before
        // their opacity is cleared.
        if let Err(error) = service.set_input_focus(state.xid, false) {
            tracing::debug!(%error, xid = state.xid, "could not clear gamescope input focus");
        }
        if let Err(error) = service.set_opacity(state.xid, 0) {
            tracing::debug!(%error, xid = state.xid, "could not hide gamescope window");
        }
    } else if state.external_overlay {
        if let Err(error) = service.set_input_focus(state.xid, false) {
            tracing::debug!(%error, xid = state.xid, "could not clear gamescope input focus");
        }
        if let Err(error) = service.set_opacity(state.xid, FULL_OPACITY) {
            tracing::debug!(%error, xid = state.xid, "could not show gamescope overlay");
        }
    } else if let Err(error) = service.set_opacity(state.xid, FULL_OPACITY) {
        // Never give input to a window that may still be invisible.
        tracing::debug!(%error, xid = state.xid, "could not show gamescope window");
    } else if let Err(error) = service.set_input_focus(state.xid, true) {
        tracing::debug!(%error, xid = state.xid, "could not set gamescope input focus");
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
        if !GAMESCOPE_SELECTED.load(Ordering::Relaxed) {
            return None;
        }
        match X11Owner::start() {
            Ok((owner, (output_width, output_height))) => {
                let scale_override = std::env::var(SCALE_ENV).unwrap_or_default();
                let requested = parse_scale(&scale_override);
                let scale = ui_scale(output_width, requested);
                match requested {
                    None if !scale_override.is_empty() => tracing::warn!(
                        value = scale_override,
                        "ignoring invalid gamescope UI scale override"
                    ),
                    Some(requested) if requested != scale => tracing::warn!(
                        requested = scale_override,
                        applied = scale,
                        min = MIN_SCALE,
                        max = MAX_SCALE,
                        "clamped gamescope UI scale override"
                    ),
                    _ => {}
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
        self.register(&window);
        *self.current.borrow_mut() = Some(window.downgrade());
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
    }

    pub fn show_auxiliary(self: &Rc<Self>, window: &gtk::Window) {
        self.register(window);
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
        self.retire_current(window);
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

    fn register(self: &Rc<Self>, window: &gtk::Window) {
        self.windows
            .borrow_mut()
            .retain(|entry| entry.window.upgrade().is_some());
        if let Some(entry) = self.windows.borrow_mut().iter_mut().find(|entry| {
            entry
                .window
                .upgrade()
                .is_some_and(|registered| registered == *window)
        }) {
            entry.hidden = false;
            return;
        }
        self.windows.borrow_mut().push(RegisteredWindow {
            window: window.downgrade(),
            hidden: false,
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
        self.retire_current(closing);
    }

    /// If `window` was current, the newest non-hidden window takes over.
    fn retire_current(&self, window: &gtk::Window) {
        if self.current_window().as_ref() == Some(window) {
            *self.current.borrow_mut() = self
                .windows
                .borrow()
                .iter()
                .rev()
                .filter(|entry| !entry.hidden)
                .find_map(|entry| entry.window.upgrade())
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

fn ui_scale(output_width: i32, requested: Option<f64>) -> f64 {
    requested
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
        let mailbox = Arc::new(StateMailbox::default());
        let thread_mailbox = Arc::clone(&mailbox);
        let thread = thread::spawn(move || owner_loop(service, &thread_mailbox));
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
            |_| true,
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
            |_| true,
        );
        assert_eq!(displays, None);
    }

    #[test]
    fn advertised_displays_must_be_live_and_primary_gamescope() {
        let runtime = std::env::temp_dir().join(format!(
            "z13helper-gamescope-advertised-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&runtime);
        fs::create_dir(&runtime).unwrap();
        let socket = runtime.join("gamescope-0");
        let resolve = |is_primary: fn(&OsStr) -> bool| {
            resolve_gamescope_displays(
                Some(&runtime),
                Some(OsStr::new("gamescope-0")),
                Some(OsStr::new(":1")),
                false,
                |_| None,
                || Some(OsString::from(":0")),
                is_primary,
            )
        };

        let listener = UnixListener::bind(&socket).unwrap();
        // A leaked desktop DISPLAY is replaced by the discovered server 0.
        assert_eq!(
            resolve(|display| display == ":0"),
            Some(GamescopeDisplays {
                wayland: OsString::from("gamescope-0"),
                x11: OsString::from(":0"),
            })
        );
        // A crashed gamescope leaves a socket file nothing accepts on.
        drop(listener);
        assert!(socket.exists());
        assert_eq!(resolve(|_| true), None);
        fs::remove_dir_all(runtime).unwrap();
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
        let scale = |width, value| ui_scale(width, parse_scale(value));
        assert!((scale(2560, "") - 2560.0 / REFERENCE_WIDTH).abs() < 0.0001);
        assert_eq!(scale(1280, ""), MIN_SCALE);
        assert_eq!(scale(7680, ""), MAX_SCALE);
        assert_eq!(scale(2560, "1.25"), 1.25);
        assert_eq!(scale(2560, "0.1"), MIN_SCALE);
        assert_eq!(scale(2560, "30"), MAX_SCALE);
        let automatic = scale(2560, "");
        for invalid in ["garbage", "0", "-2", "NaN"] {
            assert_eq!(scale(2560, invalid), automatic);
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
            external_overlay: false
        }]));
        assert_eq!(flush_events_rx.recv_timeout(Duration::from_secs(1)), Ok(1));
        assert!(mailbox.submit(&[WindowState {
            xid: 9,
            active: false,
            external_overlay: false
        }]));
        release_tx.send(()).unwrap();
        assert_eq!(flush_events_rx.recv_timeout(Duration::from_secs(1)), Ok(2));
        mailbox.close();
        thread.join().unwrap();
        let operations = operations.lock().unwrap();
        assert_eq!(
            operations.as_slice(),
            [
                "overlay:7:false",
                "opacity:7:4294967295",
                "focus:7:true",
                "overlay:7:false",
                "focus:7:false",
                "opacity:7:0",
                "overlay:9:false",
                "focus:9:false",
                "opacity:9:0",
            ]
        );
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
            external_overlay: false,
        }]);
        assert_eq!(flush_events_rx.recv_timeout(Duration::from_secs(1)), Ok(1));
        operations.lock().unwrap().clear();
        mailbox.submit(&[WindowState {
            xid: 11,
            active: false,
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

    struct OpacityFails(Vec<String>);

    impl X11Io for OpacityFails {
        fn set_overlay(&mut self, _: Window, _: bool) -> Result<()> {
            Ok(())
        }
        fn set_input_focus(&mut self, xid: Window, focused: bool) -> Result<()> {
            self.0.push(format!("focus:{xid}:{focused}"));
            Ok(())
        }
        fn set_opacity(&mut self, _: Window, _: u32) -> Result<()> {
            anyhow::bail!("opacity write failed")
        }
        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn failed_opacity_write_does_not_grant_input_focus() {
        let mut service = OpacityFails(Vec::new());
        apply_one(
            &mut service,
            WindowState {
                xid: 5,
                active: true,
                external_overlay: false,
            },
        );
        assert!(service.0.is_empty(), "{:?}", service.0);
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
}
