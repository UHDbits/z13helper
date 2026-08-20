//! Small bridge between GTK's main context and blocking daemon I/O.

use std::sync::OnceLock;
use std::thread;

type Job = Box<dyn FnOnce() + Send + 'static>;

fn queue() -> &'static async_channel::Sender<Job> {
    static QUEUE: OnceLock<async_channel::Sender<Job>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (sender, receiver) = async_channel::bounded::<Job>(16);
        thread::spawn(move || {
            while let Ok(job) = receiver.recv_blocking() {
                job();
            }
        });
        sender
    })
}

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
    let queue = queue().clone();
    glib::MainContext::default().spawn_local(async move {
        let job: Job = Box::new(move || {
            let result = work();
            let _ = sender.send_blocking(result);
        });
        let _ = queue.send(job).await;
    });
}
