use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

pub struct ExitSignal {
    notifier: Notify,
    exited: AtomicBool,
}

impl ExitSignal {
    pub fn signal(&self) {
        self.exited.store(true, Ordering::SeqCst);
        self.notifier.notify_waiters();
    }

    pub async fn wait(&self) {
        loop {
            let notified = self.notifier.notified();
            if self.exited.load(Ordering::SeqCst) {
                break;
            }
            notified.await;
        }
    }
}

impl Default for ExitSignal {
    fn default() -> Self {
        Self {
            notifier: Notify::new(),
            exited: AtomicBool::new(false),
        }
    }
}
