//! The shared session: who is signed in (epoch) and which credentials they hold
//! (credential revision).
//!
//! Two counters, because they mean different things to the realtime socket and calls:
//! - `epoch` changes on every login, logout or clear — an identity change, even logout →
//!   login as the same user. Sockets, pending commands and calls are bound to it and must
//!   not outlive it.
//! - `credential_rev` changes on every committed token refresh within an epoch. The socket
//!   re-authenticates on it; nothing else is invalidated.
//!
//! Every mutation is compare-and-set against what the caller read, so a slow refresh
//! (success *or* failure) can never overwrite or erase a newer login.

use std::sync::Arc;

use tokio::sync::{watch, RwLock};

use crate::{AuthState, Session};

/// Counters observed by the socket (and later, calls).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Revision {
    pub(crate) epoch: u64,
    pub(crate) credential_rev: u64,
}

#[derive(Default)]
struct Cell {
    rev: Revision,
    session: Option<Session>,
}

/// Outcome of applying a refresh result.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RefreshApplied {
    /// The session still held the refresh token we rotated; new credentials are live.
    Committed,
    /// A login/logout replaced the session meanwhile; the result was dropped.
    Discarded,
}

#[derive(Clone)]
pub(crate) struct SessionStore {
    cell: Arc<RwLock<Cell>>,
    /// Serializes refreshes (the periodic loop and the socket's 1008 recovery), so a
    /// token is rotated once, not once per caller.
    pub(crate) refresh_lock: Arc<tokio::sync::Mutex<()>>,
    /// After a 429 on /auth/refresh: no refresh is sent before this (every caller, not only the
    /// one that got the 429, so queued refreshes do not each try again at once).
    pub(crate) refresh_not_before: Arc<std::sync::Mutex<Option<tokio::time::Instant>>>,
    rev_tx: Arc<watch::Sender<Revision>>,
    state_tx: Arc<watch::Sender<AuthState>>,
}

impl SessionStore {
    pub(crate) fn new(state_tx: Arc<watch::Sender<AuthState>>) -> Self {
        let (rev_tx, _) = watch::channel(Revision::default());
        Self {
            cell: Arc::new(RwLock::new(Cell::default())),
            refresh_lock: Arc::default(),
            refresh_not_before: Arc::default(),
            rev_tx: Arc::new(rev_tx),
            state_tx,
        }
    }

    /// Observe epoch / credential changes.
    pub(crate) fn watch(&self) -> watch::Receiver<Revision> {
        self.rev_tx.subscribe()
    }

    /// The current session and its revision, cloned (the lock is released on return).
    pub(crate) async fn snapshot(&self) -> (Revision, Option<Session>) {
        let cell = self.cell.read().await;
        (cell.rev, cell.session.clone())
    }

    pub(crate) async fn access_token(&self) -> Option<String> {
        self.cell
            .read()
            .await
            .session
            .as_ref()
            .map(|s| s.access_token.clone())
    }

    pub(crate) async fn with_session<T>(&self, f: impl FnOnce(&Session) -> T) -> Option<T> {
        self.cell.read().await.session.as_ref().map(f)
    }

    /// Login (Some) or logout (None): always a new epoch.
    pub(crate) async fn replace(&self, session: Option<Session>) {
        let rev = {
            let mut cell = self.cell.write().await;
            cell.rev.epoch += 1;
            cell.rev.credential_rev = 0;
            cell.session = session;
            cell.rev
        };
        // A new sign-in (or sign-out) does not inherit the previous session's 429 wait.
        *self.refresh_not_before.lock().unwrap() = None;
        self.rev_tx.send_replace(rev);
    }

    /// Apply rotated tokens only if the session still holds `rotated_from`.
    pub(crate) async fn commit_refresh(
        &self,
        rotated_from: &str,
        access_token: String,
        refresh_token: String,
    ) -> RefreshApplied {
        let rev = {
            let mut cell = self.cell.write().await;
            match cell.session.as_mut() {
                Some(s) if s.refresh_token == rotated_from => {
                    s.access_token = access_token;
                    s.refresh_token = refresh_token;
                    cell.rev.credential_rev += 1;
                    cell.rev
                }
                _ => return RefreshApplied::Discarded,
            }
        };
        *self.refresh_not_before.lock().unwrap() = None; // the wait is over: it worked
        self.rev_tx.send_replace(rev);
        RefreshApplied::Committed
    }

    /// The server rejected `rejected` (4xx): clear the session — but only if it still holds
    /// that token. A stale rejection arriving after a new login must not sign the user out.
    /// Returns whether the session was cleared (and `LoggedOut` published).
    pub(crate) async fn clear_if_holds(&self, rejected: &str) -> bool {
        let rev = {
            let mut cell = self.cell.write().await;
            match cell.session.as_ref() {
                Some(s) if s.refresh_token == rejected => {
                    cell.session = None;
                    cell.rev.epoch += 1;
                    cell.rev.credential_rev = 0;
                    cell.rev
                }
                _ => return false,
            }
        };
        self.rev_tx.send_replace(rev);
        let _ = self.state_tx.send(AuthState::LoggedOut);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::User;

    fn session(n: u32) -> Session {
        Session {
            access_token: format!("access-{n}"),
            refresh_token: format!("refresh-{n}"),
            user: User {
                id: "u".into(),
                handle: "alice".into(),
                display_name: "Alice".into(),
                global_role: "member".into(),
            },
        }
    }

    fn store() -> (SessionStore, watch::Receiver<AuthState>) {
        let (tx, rx) = watch::channel(AuthState::LoggedIn(session(0).user));
        (SessionStore::new(Arc::new(tx)), rx)
    }

    #[tokio::test]
    async fn stale_rejection_after_a_new_login_keeps_the_new_session() {
        let (store, state) = store();
        store.replace(Some(session(1))).await;
        // A refresh for refresh-1 is in flight; meanwhile the user logs in again.
        store.replace(Some(session(2))).await;
        assert!(!store.clear_if_holds("refresh-1").await);
        assert_eq!(store.access_token().await.as_deref(), Some("access-2"));
        assert!(!matches!(*state.borrow(), AuthState::LoggedOut));
    }

    #[tokio::test]
    async fn rejection_of_the_current_token_clears_and_publishes_logged_out() {
        let (store, state) = store();
        store.replace(Some(session(1))).await;
        let before = store.snapshot().await.0;
        assert!(store.clear_if_holds("refresh-1").await);
        let (after, current) = store.snapshot().await;
        assert!(current.is_none());
        assert!(after.epoch > before.epoch);
        assert_eq!(*state.borrow(), AuthState::LoggedOut);
    }

    #[tokio::test]
    async fn discarded_refresh_changes_nothing_and_does_not_notify() {
        let (store, _state) = store();
        store.replace(Some(session(2))).await;
        let mut watch = store.watch();
        watch.mark_unchanged();
        let applied = store
            .commit_refresh("refresh-1", "x".into(), "y".into())
            .await;
        assert_eq!(applied, RefreshApplied::Discarded);
        assert!(!watch.has_changed().unwrap());
        assert_eq!(store.access_token().await.as_deref(), Some("access-2"));
    }

    #[tokio::test]
    async fn committed_refresh_bumps_credentials_not_epoch() {
        let (store, _state) = store();
        store.replace(Some(session(1))).await;
        let before = store.snapshot().await.0;
        let applied = store
            .commit_refresh("refresh-1", "access-9".into(), "refresh-9".into())
            .await;
        assert_eq!(applied, RefreshApplied::Committed);
        let after = store.snapshot().await.0;
        assert_eq!(after.epoch, before.epoch);
        assert_eq!(after.credential_rev, before.credential_rev + 1);
        assert_eq!(store.access_token().await.as_deref(), Some("access-9"));
    }

    #[tokio::test]
    async fn logout_then_login_as_the_same_user_is_a_new_epoch() {
        let (store, _state) = store();
        store.replace(Some(session(1))).await;
        let first = store.snapshot().await.0.epoch;
        store.replace(None).await;
        store.replace(Some(session(1))).await;
        assert!(store.snapshot().await.0.epoch > first + 1);
    }
}
