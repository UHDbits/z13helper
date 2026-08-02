use std::sync::mpsc::Sender;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SleepEvent {
    Sleeping,
    Resumed,
}

pub fn spawn_resume_watcher(sender: Sender<SleepEvent>) {
    std::thread::spawn(move || {
        let result = (|| -> Result<(), zbus::Error> {
            let connection = zbus::blocking::Connection::system()?;
            let proxy = zbus::blocking::Proxy::new(
                &connection,
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
            )?;
            for message in proxy.receive_signal("PrepareForSleep")? {
                if let Ok((sleeping,)) = message.body().deserialize::<(bool,)>() {
                    let _ = sender.send(if sleeping {
                        SleepEvent::Sleeping
                    } else {
                        SleepEvent::Resumed
                    });
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(%error, "resume watcher unavailable");
        }
    });
}
