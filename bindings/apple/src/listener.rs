//! Auth-state observation across the FFI.
//!
//! UniFFI cannot carry a `tokio::sync::watch::Receiver`, so Swift registers a listener
//! and gets called back. Contract — **latest state wins**:
//! - the current state is delivered first, then each change;
//! - `watch` is single-slot, so a slow listener may skip intermediate states (it may
//!   never see `Authenticating`), but observed order never regresses and the final state
//!   is always delivered. UIs render the current state; they must not count transitions.
//! - callbacks come one at a time, from a runtime worker thread (not the main thread).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use brook_core::AuthState;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::runtime::runtime;
use crate::types::FfiAuthState;

/// Implemented in Swift; wrapped there into an observable.
#[uniffi::export(with_foreign)]
pub trait AuthStateListener: Send + Sync {
    fn on_state(&self, state: FfiAuthState);
}

/// A live listener registration. Cancelled on `cancel()` or when dropped.
#[derive(uniffi::Object)]
pub struct Subscription {
    cancelled: Arc<AtomicBool>,
    task: Mutex<Option<JoinHandle<()>>>,
}

#[uniffi::export]
impl Subscription {
    /// Stop delivery. Does not wait: after it returns, **at most one** further callback may
    /// arrive (one whose check passed just before the flag was set), never two. Blocking
    /// until an in-flight callback finished would need a lock shared with the callback, and
    /// could deadlock against a Swift callback that hops synchronously to the main thread
    /// while the main thread is cancelling. Swift drops states delivered after it cancelled.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.lock().unwrap_or_else(|p| p.into_inner()).take() {
            task.abort();
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Deliver `rx`'s states to `listener` until cancelled or the sender is gone.
pub(crate) fn subscribe_receiver(
    mut rx: watch::Receiver<AuthState>,
    listener: Arc<dyn AuthStateListener>,
) -> Arc<Subscription> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    let task = runtime().spawn(async move {
        loop {
            // `borrow_and_update` marks the value seen, so the following `changed()` only
            // fires for a newer one (plain `borrow` would re-deliver it). Clone, then drop
            // the borrow before calling foreign code: no lock is held across the FFI.
            let state: FfiAuthState = rx.borrow_and_update().clone().into();
            if flag.load(Ordering::SeqCst) {
                break;
            }
            listener.on_state(state);
            if rx.changed().await.is_err() {
                break; // sender dropped: the client is gone
            }
        }
    });
    Arc::new(Subscription {
        cancelled,
        task: Mutex::new(Some(task)),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::Duration;

    use brook_core::User;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);
    // Only used to assert that something does NOT happen; never for ordering.
    const QUIET: Duration = Duration::from_millis(300);

    fn user() -> User {
        serde_json::from_value(serde_json::json!({
            "id": "u1", "handle": "alice", "display_name": "Alice", "global_role": "admin"
        }))
        .unwrap()
    }

    /// Records every callback; optionally blocks inside the first one until released.
    struct Recorder {
        seen: mpsc::Sender<FfiAuthState>,
        calls: Arc<AtomicUsize>,
        gate: Option<(mpsc::Sender<()>, Mutex<mpsc::Receiver<()>>)>,
    }

    impl AuthStateListener for Recorder {
        fn on_state(&self, state: FfiAuthState) {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let _ = self.seen.send(state);
            if let (0, Some((entered, release))) = (n, &self.gate) {
                let _ = entered.send(());
                let _ = release.lock().unwrap().recv_timeout(WAIT);
            }
        }
    }

    struct Harness {
        seen: mpsc::Receiver<FfiAuthState>,
        calls: Arc<AtomicUsize>,
        entered: Option<mpsc::Receiver<()>>,
        release: Option<mpsc::Sender<()>>,
    }

    fn recorder(blocking_first: bool) -> (Arc<Recorder>, Harness) {
        let (seen_tx, seen) = mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let (gate, entered, release) = if blocking_first {
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            (
                Some((entered_tx, Mutex::new(release_rx))),
                Some(entered_rx),
                Some(release_tx),
            )
        } else {
            (None, None, None)
        };
        let rec = Arc::new(Recorder {
            seen: seen_tx,
            calls: Arc::clone(&calls),
            gate,
        });
        (
            rec,
            Harness {
                seen,
                calls,
                entered,
                release,
            },
        )
    }

    fn logged_in() -> FfiAuthState {
        FfiAuthState::LoggedIn {
            user: user().into(),
        }
    }

    /// Test 4: a listener stuck in a callback while states change still ends on the final
    /// state, and never observes a regression.
    #[test]
    fn stalled_listener_still_receives_the_final_state_in_order() {
        let (tx, rx) = watch::channel(AuthState::LoggedOut);
        let (rec, h) = recorder(true);
        let _sub = subscribe_receiver(rx, rec);

        h.entered.as_ref().unwrap().recv_timeout(WAIT).unwrap();
        tx.send(AuthState::Authenticating).unwrap();
        tx.send(AuthState::LoggedIn(user())).unwrap();
        h.release.as_ref().unwrap().send(()).unwrap();

        let mut observed = vec![];
        while observed.last() != Some(&logged_in()) {
            observed.push(
                h.seen
                    .recv_timeout(WAIT)
                    .expect("final state never delivered"),
            );
        }
        let rank = |s: &FfiAuthState| match s {
            FfiAuthState::LoggedOut => 0,
            FfiAuthState::Authenticating => 1,
            FfiAuthState::LoggedIn { .. } | FfiAuthState::Failed { .. } => 2,
        };
        assert!(
            observed.windows(2).all(|w| rank(&w[0]) <= rank(&w[1])),
            "order regressed: {observed:?}"
        );
    }

    /// Test 5: a value that is already pending when the subscription starts is delivered
    /// once, not twice (the `borrow` vs `borrow_and_update` difference).
    #[test]
    fn pending_value_at_subscribe_time_is_not_delivered_twice() {
        let (tx, rx) = watch::channel(AuthState::LoggedOut);
        tx.send(AuthState::Authenticating).unwrap(); // unseen by `rx` when we subscribe
        let (rec, h) = recorder(false);
        let _sub = subscribe_receiver(rx, rec);

        assert_eq!(
            h.seen.recv_timeout(WAIT).unwrap(),
            FfiAuthState::Authenticating
        );
        assert!(
            h.seen.recv_timeout(QUIET).is_err(),
            "the pending value was delivered twice"
        );
        tx.send(AuthState::LoggedIn(user())).unwrap();
        assert_eq!(h.seen.recv_timeout(WAIT).unwrap(), logged_in());
    }

    /// Test 6: with no change at all, the current state is still delivered, exactly once.
    #[test]
    fn fresh_subscription_delivers_the_current_state_once() {
        let (_tx, rx) = watch::channel(AuthState::LoggedOut);
        let (rec, h) = recorder(false);
        let _sub = subscribe_receiver(rx, rec);

        assert_eq!(h.seen.recv_timeout(WAIT).unwrap(), FfiAuthState::LoggedOut);
        assert!(h.seen.recv_timeout(QUIET).is_err());
    }

    /// Test 7: cancel while a callback is in flight; states sent afterwards — each given
    /// time to be delivered — produce at most one late callback, never two.
    #[test]
    fn cancel_allows_at_most_one_late_callback() {
        let (tx, rx) = watch::channel(AuthState::LoggedOut);
        let (rec, h) = recorder(true);
        let sub = subscribe_receiver(rx, rec);

        h.entered.as_ref().unwrap().recv_timeout(WAIT).unwrap();
        // Drain the in-flight initial state, so each wait below can only see a late one.
        assert_eq!(h.seen.recv_timeout(WAIT).unwrap(), FfiAuthState::LoggedOut);
        sub.cancel();
        let at_cancel = h.calls.load(Ordering::SeqCst);
        h.release.as_ref().unwrap().send(()).unwrap();

        // `send_replace`: after cancel the task (and its receiver) may be gone, and
        // `send` errors when no receiver is left — that is the correct outcome here.
        tx.send_replace(AuthState::Authenticating);
        let _ = h.seen.recv_timeout(QUIET);
        tx.send_replace(AuthState::LoggedIn(user()));
        let _ = h.seen.recv_timeout(QUIET);

        let late = h.calls.load(Ordering::SeqCst) - at_cancel;
        assert!(late <= 1, "{late} callbacks after cancel() returned");
    }
}
