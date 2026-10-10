//! Small bridge between GTK's main context and blocking daemon I/O.

use std::sync::OnceLock;
use std::thread;

type Job = Box<dyn FnOnce() + Send + 'static>;

fn queue() -> &'static async_channel::Sender<Job> {
    static QUEUE: OnceLock<async_channel::Sender<Job>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        // Unbounded with a synchronous send: jobs run in call order.
        let (sender, receiver) = async_channel::unbounded::<Job>();
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
    let _ = queue().try_send(Box::new(move || {
        let _ = sender.send_blocking(work());
    }));
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn jobs_keep_submission_order_and_results_return_on_main_context() {
        let context = glib::MainContext::default();
        let _owner = context.acquire().unwrap();
        let (ran_tx, ran_rx) = mpsc::channel();
        let done = Rc::new(RefCell::new(Vec::new()));
        // More jobs than any bounded queue slack, submitted without letting
        // the main loop run: order must be fixed at the call, not by polling.
        for index in 0..40 {
            let ran_tx = ran_tx.clone();
            let done = Rc::clone(&done);
            super::blocking(
                move || {
                    ran_tx.send(index).unwrap();
                    index
                },
                move |index| done.borrow_mut().push(index),
            );
        }
        let ran: Vec<i32> = (0..40)
            .map(|_| ran_rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect();
        assert_eq!(ran, (0..40).collect::<Vec<_>>());

        let deadline = Instant::now() + Duration::from_secs(5);
        while done.borrow().len() < 40 && Instant::now() < deadline {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(*done.borrow(), (0..40).collect::<Vec<_>>());
    }
}
