//! Small bridge between GTK's main context and blocking daemon I/O.

use std::thread;

/// Run blocking work away from the GTK thread and deliver its result on GTK's
/// main context. The callback always runs on the UI thread.
pub fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + 'static,
) {
    let (sender, receiver) = async_channel::bounded(1);
    glib::MainContext::default().spawn_local(async move {
        if let Ok(result) = receiver.recv().await {
            done(result);
        }
    });
    thread::spawn(move || {
        let result = work();
        let _ = sender.send_blocking(result);
    });
}
