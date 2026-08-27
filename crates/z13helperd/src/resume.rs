use std::sync::mpsc::SyncSender;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SleepEvent {
    Sleeping,
    Resumed,
}

pub fn spawn_resume_watcher(sender: SyncSender<SleepEvent>, terminate: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !terminate.load(Ordering::Relaxed) {
            let result = (|| -> Result<(), zbus::Error> {
                let connection = zbus::blocking::Connection::system()?;
                let proxy = zbus::blocking::Proxy::new(
                    &connection,
                    "org.freedesktop.login1",
                    "/org/freedesktop/login1",
                    "org.freedesktop.login1.Manager",
                )?;
                for message in proxy.receive_signal("PrepareForSleep")? {
                    if terminate.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Ok((sleeping,)) = message.body().deserialize::<(bool,)>()
                        && sender
                            .send(if sleeping {
                                SleepEvent::Sleeping
                            } else {
                                SleepEvent::Resumed
                            })
                            .is_err()
                    {
                        return Ok(());
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                tracing::warn!(%error, "resume watcher reconnecting");
            }
            if !terminate.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    });
}
