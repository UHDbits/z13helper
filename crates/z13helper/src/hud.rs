//! G-Helper-style HUD toast for power-source / profile switches.
//!
//! Uses gtk4-layer-shell on Wayland (click-through via empty input region).
//! Under gamescope, use its non-interactive external-overlay plane.
//! Fall back to org.freedesktop.Notifications when neither overlay path works.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use gtk::prelude::*;
use gtk4 as gtk;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::app::AppState;

pub fn show(state: &Rc<AppState>, profile_name: &str, on_battery: bool) {
    let glyph = if on_battery { "🔋" } else { "🔌" };
    let message = format!("{glyph}  {profile_name}");

    if try_popup(state, &message) {
        return;
    }
    state.notification_owner.submit(&message);
}

fn try_popup(state: &Rc<AppState>, message: &str) -> bool {
    let gamescope = state.gamescope.as_ref();
    let layer_shell = gtk4_layer_shell::is_supported();
    if gamescope.is_none() && !layer_shell {
        return false;
    }

    let window = gtk::Window::builder()
        .application(state.app.upcast_ref::<gtk::Application>())
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

    if let Some(gamescope) = gamescope {
        gamescope.prepare_hud(&window);
    }

    if layer_shell {
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_anchor(Edge::Bottom, true);
        window.set_margin(Edge::Bottom, 80);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(-1);
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

#[derive(Default)]
struct NotificationMailbox {
    latest: Mutex<Option<String>>,
    wake: Mutex<Option<mpsc::SyncSender<()>>>,
    closed: AtomicBool,
}

impl NotificationMailbox {
    fn submit(&self, message: &str) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        let Ok(mut latest) = self.latest.lock() else {
            return false;
        };
        *latest = Some(message.to_owned());
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

    fn take(&self) -> Option<String> {
        self.latest.lock().ok()?.take()
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

trait NotificationSink: Send {
    fn notify(&mut self, message: &str);
}

struct ZbusNotificationSink {
    connection: Option<zbus::blocking::Connection>,
}

impl ZbusNotificationSink {
    fn connect() -> Self {
        Self {
            connection: zbus::blocking::Connection::session().ok(),
        }
    }
}

impl NotificationSink for ZbusNotificationSink {
    fn notify(&mut self, message: &str) {
        if self.connection.is_none() {
            self.connection = zbus::blocking::Connection::session().ok();
        }
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        if connection
            .call_method(
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
                    HashMap::<String, zbus::zvariant::Value>::new(),
                    2000i32,
                ),
            )
            .is_err()
        {
            self.connection = None;
        }
    }
}

pub struct NotificationOwner {
    mailbox: Arc<NotificationMailbox>,
    thread: Option<JoinHandle<()>>,
}

impl NotificationOwner {
    pub fn start() -> Self {
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let mailbox = Arc::new(NotificationMailbox::default());
        let thread_mailbox = Arc::clone(&mailbox);
        let thread = match thread::Builder::new()
            .name("z13helper-hud-notifications".into())
            .spawn(move || {
                notification_loop(ZbusNotificationSink::connect(), thread_mailbox, wake_rx)
            }) {
            Ok(thread) => thread,
            Err(error) => {
                tracing::warn!(%error, "could not start HUD notification owner; disabling fallback notifications");
                return Self {
                    mailbox,
                    thread: None,
                };
            }
        };
        *mailbox.wake.lock().expect("new HUD notification mailbox") = Some(wake_tx);
        Self {
            mailbox,
            thread: Some(thread),
        }
    }

    fn submit(&self, message: &str) {
        if !self.mailbox.submit(message) {
            tracing::debug!("HUD notification owner is stopping");
        }
    }
}

impl Drop for NotificationOwner {
    fn drop(&mut self) {
        self.mailbox.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn notification_loop<S: NotificationSink>(
    mut sink: S,
    mailbox: Arc<NotificationMailbox>,
    wake_rx: mpsc::Receiver<()>,
) {
    while wake_rx.recv().is_ok() {
        if mailbox.closed.load(Ordering::Acquire) {
            return;
        }
        while let Some(message) = mailbox.take() {
            sink.notify(&message);
            if mailbox.closed.load(Ordering::Acquire) || !has_pending_notification(&mailbox) {
                break;
            }
        }
    }
}

fn has_pending_notification(mailbox: &NotificationMailbox) -> bool {
    mailbox
        .latest
        .try_lock()
        .map(|latest| latest.is_some())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct FakeSink {
        seen: Arc<Mutex<Vec<String>>>,
        seen_events: mpsc::Sender<usize>,
        first_started: Option<mpsc::Sender<()>>,
        release_first: Option<mpsc::Receiver<()>>,
    }

    struct RecoveringSink {
        available: Arc<AtomicBool>,
        delivered: Arc<Mutex<Vec<String>>>,
        attempts: Arc<Mutex<usize>>,
        attempt_events: mpsc::Sender<usize>,
        delivered_event: mpsc::Sender<()>,
    }

    impl NotificationSink for RecoveringSink {
        fn notify(&mut self, message: &str) {
            *self.attempts.lock().unwrap() += 1;
            let _ = self.attempt_events.send(*self.attempts.lock().unwrap());
            if self.available.load(Ordering::Acquire) {
                self.delivered.lock().unwrap().push(message.to_owned());
                let _ = self.delivered_event.send(());
            }
        }
    }

    impl NotificationSink for FakeSink {
        fn notify(&mut self, message: &str) {
            if let Some(started) = self.first_started.take() {
                let _ = started.send(());
                if let Some(release) = self.release_first.take() {
                    let _ = release.recv();
                }
            }
            let mut seen = self.seen.lock().unwrap();
            seen.push(message.to_owned());
            let _ = self.seen_events.send(seen.len());
        }
    }

    fn fake_owner<S: NotificationSink + 'static>(
        sink: S,
    ) -> (Arc<NotificationMailbox>, JoinHandle<()>) {
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let mailbox = Arc::new(NotificationMailbox::default());
        *mailbox.wake.lock().unwrap() = Some(wake_tx);
        let thread_mailbox = Arc::clone(&mailbox);
        let thread = thread::spawn(move || notification_loop(sink, thread_mailbox, wake_rx));
        (mailbox, thread)
    }

    #[test]
    fn notification_burst_keeps_only_the_latest_pending_message() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (first_started_tx, first_started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (seen_events_tx, seen_events_rx) = mpsc::channel();
        let (mailbox, thread) = fake_owner(FakeSink {
            seen: Arc::clone(&seen),
            seen_events: seen_events_tx,
            first_started: Some(first_started_tx),
            release_first: Some(release_rx),
        });

        assert!(mailbox.submit("first"));
        first_started_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(mailbox.submit("dropped"));
        assert!(mailbox.submit("latest"));
        release_tx.send(()).unwrap();
        assert_eq!(seen_events_rx.recv_timeout(Duration::from_secs(1)), Ok(1));
        assert_eq!(seen_events_rx.recv_timeout(Duration::from_secs(1)), Ok(2));
        mailbox.close();
        thread.join().unwrap();
        assert_eq!(seen.lock().unwrap().as_slice(), ["first", "latest"]);
    }

    #[test]
    fn notification_owner_shutdown_is_joined_without_processing_after_close() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (mailbox, thread) = fake_owner(FakeSink {
            seen: Arc::clone(&seen),
            seen_events: mpsc::channel().0,
            first_started: None,
            release_first: None,
        });
        mailbox.close();
        thread.join().unwrap();
        assert!(seen.lock().unwrap().is_empty());
        assert!(!mailbox.submit("after shutdown"));
    }

    #[test]
    fn notification_owner_keeps_retrying_after_sink_recovery() {
        let available = Arc::new(AtomicBool::new(false));
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let attempts = Arc::new(Mutex::new(0));
        let (attempt_events_tx, attempt_events_rx) = mpsc::channel();
        let (delivered_event_tx, delivered_event_rx) = mpsc::channel();
        let (mailbox, thread) = fake_owner(RecoveringSink {
            available: Arc::clone(&available),
            delivered: Arc::clone(&delivered),
            attempts: Arc::clone(&attempts),
            attempt_events: attempt_events_tx,
            delivered_event: delivered_event_tx,
        });

        mailbox.submit("while disconnected");
        assert_eq!(
            attempt_events_rx.recv_timeout(Duration::from_secs(1)),
            Ok(1)
        );
        available.store(true, Ordering::Release);
        mailbox.submit("after reconnect");
        delivered_event_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        mailbox.close();
        thread.join().unwrap();
        assert_eq!(delivered.lock().unwrap().as_slice(), ["after reconnect"]);
        assert_eq!(*attempts.lock().unwrap(), 2);
    }
}
