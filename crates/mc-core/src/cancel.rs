//! Stopping a turn that is already running.
//!
//! A turn can sit in three places when the user asks it to stop: waiting on the
//! model's response, running a command, or between tool rounds. A flag alone
//! would only be seen at the third, so this pairs the flag with a notification
//! the other two can wait on.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::Notify;

#[derive(Debug, Clone, Default)]
pub struct Cancel {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    flagged: AtomicBool,
    notify: Notify,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the current turn to stop. Safe to call from any thread, and harmless
    /// when nothing is running.
    pub fn cancel(&self) {
        self.inner.flagged.store(true, Ordering::SeqCst);
        // `notify_waiters` only wakes waiters that already exist, so anything
        // that starts waiting later relies on the flag above.
        self.inner.notify.notify_waiters();
    }

    /// Clear the flag, ready for the next turn.
    pub fn reset(&self) {
        self.inner.flagged.store(false, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.flagged.load(Ordering::SeqCst)
    }

    /// Resolves when cancelled - including if that already happened.
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            // Register before re-checking, so a cancel between the check and
            // the wait cannot be missed.
            let waiting = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            waiting.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn waiting_wakes_when_cancelled() {
        let cancel = Cancel::new();
        let waiter = cancel.clone();
        let task = tokio::spawn(async move { waiter.cancelled().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("cancelled() should resolve")
            .unwrap();
    }

    #[tokio::test]
    async fn cancelling_before_waiting_is_not_missed() {
        let cancel = Cancel::new();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), cancel.cancelled())
            .await
            .expect("an already-cancelled token resolves immediately");
    }

    #[tokio::test]
    async fn reset_lets_the_next_turn_run() {
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(cancel.is_cancelled());
        cancel.reset();
        assert!(!cancel.is_cancelled());
        // And it must not resolve any more.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), cancel.cancelled())
                .await
                .is_err()
        );
    }
}
