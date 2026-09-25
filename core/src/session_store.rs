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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use serde_json::json;
use tokio::runtime::Handle;
use tokio::sync::{watch, RwLock};
use url::Url;

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
    /// The current login attempt. `login` reserves one before it waits for anything; `logout`,
    /// a newer login and `close` move past it, so a stale login installs nothing.
    login_gen: u64,
    /// The current attempt is waiting for its TOTP step (its challenge is open). Closed by a
    /// success, `cancel_totp`, an expired challenge, and anything that moves `login_gen`.
    challenge_open: bool,
    /// The current attempt's process-wide ticket (see `persist::ticket`): what it may touch in
    /// the shared slot.
    ticket: u64,
    /// The ticket of the attempt that installed the current session: what its rotations and
    /// its rejection may touch.
    session_ticket: u64,
}

/// Revoking refresh tokens core stops holding, from anywhere, including a `Drop` on a thread
/// outside the runtime: spawned through a handle captured on the runtime, never ambiently.
#[derive(Default)]
struct Detached {
    runtime: OnceLock<Handle>,
    http: OnceLock<(reqwest::Client, Url)>,
}

/// Outcome of applying a refresh result.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RefreshApplied {
    /// The session still held the refresh token we rotated; new credentials are live.
    Committed,
    /// A login/logout replaced the session meanwhile; the result was dropped.
    Discarded,
    /// The client is gone, but the stored copy followed the rotation (quit keeps the session):
    /// the new token must not be revoked.
    Stored,
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
    pub(crate) state_tx: Arc<watch::Sender<AuthState>>,
    detached: Arc<Detached>,
    /// Set synchronously when the client is dropped, and read inside every install or commit's
    /// write section: nothing lands in an abandoned store, even before its cleanup task runs.
    closed: Arc<AtomicBool>,
    /// Unique per store (per client): what binds a TOTP challenge to the client it came from.
    id: u64,
    /// Staying signed in: the stored copy, written through inside the write sections below.
    persistence: Arc<OnceLock<crate::persist::Persistence>>,
    /// Whether the last sign-out made the stored copy unusable (deleted or fenced).
    sign_out_complete: Arc<AtomicBool>,
}

static NEXT_STORE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl SessionStore {
    pub(crate) fn new(state_tx: Arc<watch::Sender<AuthState>>) -> Self {
        let (rev_tx, _) = watch::channel(Revision::default());
        Self {
            cell: Arc::new(RwLock::new(Cell::default())),
            refresh_lock: Arc::default(),
            refresh_not_before: Arc::default(),
            rev_tx: Arc::new(rev_tx),
            state_tx,
            detached: Arc::default(),
            closed: Arc::default(),
            id: NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed),
            persistence: Arc::default(),
            sign_out_complete: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(crate) fn set_persistence(&self, p: crate::persist::Persistence) {
        let _ = self.persistence.set(p);
    }

    pub(crate) fn persistence(&self) -> Option<&crate::persist::Persistence> {
        self.persistence.get()
    }

    pub(crate) fn sign_out_complete(&self) -> bool {
        self.sign_out_complete.load(Ordering::SeqCst)
    }

    /// A restore's stored token was refused: delete the stored copy only if it still holds that
    /// token, checked inside the write section (a newer sign-in may have replaced it).
    pub(crate) async fn clear_persisted_if_holds(&self, gen: u64, refresh_token: &str) {
        let cell = self.cell.write().await;
        if cell.login_gen != gen {
            return; // this restore is stale: whatever replaced it decides
        }
        if let Some(p) = self.persistence.get() {
            p.guarded(cell.ticket, false, |p| p.clear_if_holds(refresh_token));
        }
    }

    /// This store's identity (a challenge from another client never matches it).
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Where detached revokes go (set once, by the client).
    pub(crate) fn set_revoke_target(&self, http: reqwest::Client, base: Url) {
        let _ = self.detached.http.set((http, base));
    }

    /// Capture the runtime this store's work runs on. Called from the client's async entry
    /// points (they always run inside a runtime: reqwest needs one); a store that never saw
    /// one never held a session, so it never has anything to revoke.
    pub(crate) fn note_runtime(&self) {
        if self.detached.runtime.get().is_none() {
            if let Ok(handle) = Handle::try_current() {
                let _ = self.detached.runtime.set(handle);
            }
        }
    }

    /// Best-effort `POST /auth/logout` for a refresh token core will not keep, in its own task
    /// (bounded by the client's request timeout); errors are logged by kind only.
    pub(crate) fn revoke_detached(&self, refresh_token: String) {
        let (Some(runtime), Some((http, base))) =
            (self.detached.runtime.get(), self.detached.http.get())
        else {
            return;
        };
        let (http, base) = (http.clone(), base.clone());
        runtime.spawn(async move {
            let Ok(url) = base.join("api/v1/auth/logout") else {
                return;
            };
            let sent = http
                .post(url)
                .json(&json!({ "refresh_token": refresh_token }))
                .send()
                .await;
            if let Err(err) = sent {
                tracing::warn!(
                    timeout = err.is_timeout(),
                    connect = err.is_connect(),
                    "revoking a refresh token failed"
                );
            }
        });
    }

    /// Start a login attempt; any earlier one becomes stale.
    pub(crate) async fn reserve_login(&self) -> u64 {
        let mut cell = self.cell.write().await;
        cell.login_gen += 1;
        cell.challenge_open = false;
        cell.ticket = crate::persist::ticket();
        cell.login_gen
    }

    /// Publish a failed login, only if attempt `gen` is still current, under the same lock
    /// that sign-out takes: a sign-out's `LoggedOut` is never overwritten by a stale failure.
    pub(crate) async fn publish_failed_if_current(&self, gen: u64, message: String) {
        let cell = self.cell.write().await;
        if !self.is_closed() && cell.login_gen == gen {
            let _ = self.state_tx.send(AuthState::Failed(message));
        }
    }

    /// For login attempt `gen`: take the current session out (a new epoch) and return it, or
    /// `Err` if the attempt is stale. The caller revokes what it took.
    pub(crate) async fn take_out_for_login(&self, gen: u64) -> Result<Option<Session>, ()> {
        let (rev, old) = {
            let mut cell = self.cell.write().await;
            if self.is_closed() || cell.login_gen != gen {
                return Err(());
            }
            let old = cell.session.take();
            if old.is_some() {
                // The displaced sign-in must not be restorable, whatever this login's outcome.
                if let Some(p) = self.persistence.get() {
                    p.guarded(cell.ticket, true, crate::persist::Persistence::clear);
                }
            }
            cell.rev.epoch += 1;
            cell.rev.credential_rev = 0;
            (cell.rev, old)
        };
        *self.refresh_not_before.lock().unwrap() = None;
        self.rev_tx.send_replace(rev);
        Ok(old)
    }

    /// Attempt `gen` got a TOTP challenge: open it (false if the attempt is already stale).
    pub(crate) async fn open_challenge(&self, gen: u64) -> bool {
        let mut cell = self.cell.write().await;
        if self.is_closed() || cell.login_gen != gen {
            return false;
        }
        cell.challenge_open = true;
        true
    }

    /// Whether `gen`'s challenge is still open and current.
    pub(crate) async fn challenge_current(&self, gen: u64) -> bool {
        let cell = self.cell.read().await;
        !self.is_closed() && cell.login_gen == gen && cell.challenge_open
    }

    /// Install the session a completed challenge `gen` produced, closing the challenge and
    /// publishing `LoggedIn` in the same write. False: no longer current; revoke the pair.
    pub(crate) async fn install_for_challenge(&self, gen: u64, session: Session) -> bool {
        let mut installed = false;
        let rev = {
            let mut cell = self.cell.write().await;
            let owns = |cell: &Cell| match self.persistence.get() {
                // Claims the slot; refused when a newer attempt (in any client) owns it.
                Some(p) => p
                    .guarded(cell.ticket, true, |p| p.write(&session))
                    .is_some(),
                None => true,
            };
            if !self.is_closed() && cell.login_gen == gen && cell.challenge_open && owns(&cell) {
                let user = session.user.clone();
                cell.challenge_open = false;
                cell.session_ticket = cell.ticket;
                cell.session = Some(session);
                cell.rev.epoch += 1;
                cell.rev.credential_rev = 0;
                let _ = self.state_tx.send(AuthState::LoggedIn(user));
                installed = true;
            }
            cell.rev
        };
        if installed {
            *self.refresh_not_before.lock().unwrap() = None;
            self.rev_tx.send_replace(rev);
        }
        installed
    }

    /// End challenge `gen` if it is still the open, current one (Back, or the server said it
    /// expired): the attempt is over and `LoggedOut` is published. Otherwise nothing.
    pub(crate) async fn end_challenge(&self, gen: u64) {
        let mut cell = self.cell.write().await;
        if !self.is_closed() && cell.login_gen == gen && cell.challenge_open {
            cell.challenge_open = false;
            cell.login_gen += 1;
            let _ = self.state_tx.send(AuthState::LoggedOut);
        }
    }

    /// Install login attempt `gen`'s session and publish `LoggedIn`, in the same write that
    /// checks the attempt is still current (a sign-out can't slip between install and publish).
    /// False: stale; the caller revokes the pair.
    pub(crate) async fn install_for_login(&self, gen: u64, session: Session) -> bool {
        let rev = {
            let mut cell = self.cell.write().await;
            if self.is_closed() || cell.login_gen != gen {
                return false;
            }
            if let Some(p) = self.persistence.get() {
                // Claims the slot; refused when a newer attempt (in any client) owns it, so a
                // late restore can't bring back a sign-in that was replaced or signed out.
                if p.guarded(cell.ticket, true, |p| p.write(&session))
                    .is_none()
                {
                    return false;
                }
            }
            let user = session.user.clone();
            cell.session_ticket = cell.ticket;
            cell.session = Some(session);
            cell.rev.epoch += 1;
            cell.rev.credential_rev = 0;
            let _ = self.state_tx.send(AuthState::LoggedIn(user));
            cell.rev
        };
        *self.refresh_not_before.lock().unwrap() = None;
        self.rev_tx.send_replace(rev);
        true
    }

    /// Sign out (or, with `close`, the client is gone): end any login attempt, take the
    /// session out and publish `LoggedOut`. Never waits for the refresh lock. Returns the
    /// session taken, for the caller to revoke.
    pub(crate) async fn sign_out(&self, close: bool) -> Option<Session> {
        let (rev, old) = {
            let mut cell = self.cell.write().await;
            cell.login_gen += 1;
            cell.challenge_open = false;
            if close {
                self.closed.store(true, Ordering::SeqCst);
            }
            let old = cell.session.take();
            // A sign-out clears the stored copy before it returns; a close (quit) keeps it.
            if !close {
                if let Some(p) = self.persistence.get() {
                    // A newer attempt elsewhere owns the slot: nothing of ours is stored.
                    let done = p.guarded(cell.ticket, false, crate::persist::Persistence::clear);
                    self.sign_out_complete
                        .store(done.unwrap_or(true), Ordering::SeqCst);
                }
            }
            cell.rev.epoch += 1;
            cell.rev.credential_rev = 0;
            let _ = self.state_tx.send(AuthState::LoggedOut); // under the lock (see install)
            (cell.rev, old)
        };
        *self.refresh_not_before.lock().unwrap() = None;
        self.rev_tx.send_replace(rev);
        old
    }

    /// The client is being dropped: fence the store and revoke what it held, in a task on the
    /// captured runtime (a `Drop` may run on any thread, outside the runtime).
    pub(crate) fn close_detached(&self) {
        // The fence first, synchronously: from here on nothing installs or commits.
        self.closed.store(true, Ordering::SeqCst);
        let Some(runtime) = self.detached.runtime.get() else {
            return; // never ran on a runtime: never held a session
        };
        let store = self.clone();
        runtime.spawn(async move {
            if let Some(old) = store.sign_out(true).await {
                // Quit isn't sign-out: with a stored session, the token stays valid for the
                // next launch. Without one, a dropped client is a signed-out one.
                if store.persistence.get().is_none() {
                    store.revoke_detached(old.refresh_token);
                }
            }
        });
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

    /// Swap the session in one step (a new epoch): tests use it to stand in for another sign-in.
    /// Production goes through `take_out_for_login` / `install_for_login` / `sign_out`.
    #[cfg(test)]
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
            if self.is_closed() {
                // Quitting mid-refresh: the server already rotated, so the stored token is
                // dead. Store the new one (if ours is still the stored copy) rather than
                // revoke it, or the next launch finds a dead session.
                let stored = self.persistence.get().and_then(|p| {
                    p.guarded(cell.session_ticket, false, |p| {
                        p.follow_rotation(rotated_from, &refresh_token)
                    })
                });
                return if stored == Some(true) {
                    RefreshApplied::Stored
                } else {
                    RefreshApplied::Discarded
                };
            }
            let ticket = cell.session_ticket;
            match cell.session.as_mut() {
                Some(s) if s.refresh_token == rotated_from => {
                    s.access_token = access_token;
                    s.refresh_token = refresh_token;
                    if let Some(p) = self.persistence.get() {
                        // Rotation made the stored token dead: follow it (unless a newer
                        // sign-in, in another client, owns the slot now).
                        p.guarded(ticket, false, |p| p.write(s));
                    }
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
                    if let Some(p) = self.persistence.get() {
                        // A remote sign-out: not restorable either (only if still ours).
                        p.guarded(
                            cell.session_ticket,
                            false,
                            crate::persist::Persistence::clear,
                        );
                    }
                    cell.session = None;
                    cell.rev.epoch += 1;
                    cell.rev.credential_rev = 0;
                    // Under the lock, like every publish: a login installing right after this
                    // clear publishes its LoggedIn after, never before, this LoggedOut.
                    let _ = self.state_tx.send(AuthState::LoggedOut);
                    cell.rev
                }
                _ => return false,
            }
        };
        self.rev_tx.send_replace(rev);
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

    /// An install and a sign-out queued back to back on the store (the lock is fair): the
    /// sign-out comes second, so the published state must end `LoggedOut`. If the install
    /// published after releasing the lock, its stale `LoggedIn` would land last.
    #[tokio::test]
    async fn a_sign_out_right_after_an_install_is_the_last_state_published() {
        let (store, state) = store();
        let gen = store.reserve_login().await;
        let held = store.cell.write().await;
        let s = store.clone();
        let install = tokio::spawn(async move { s.install_for_login(gen, session(1)).await });
        tokio::task::yield_now().await; // the install waits on the lock
        let s = store.clone();
        let out = tokio::spawn(async move { s.sign_out(false).await });
        tokio::task::yield_now().await; // the sign-out waits behind it
        drop(held);
        assert!(install.await.unwrap());
        out.await.unwrap();
        assert!(store.snapshot().await.1.is_none());
        assert_eq!(*state.borrow(), AuthState::LoggedOut);
    }

    /// Persistence follows store order: an install and a sign-out queued back to back leave the
    /// stored copy cleared, not rewritten by a late install.
    #[tokio::test]
    async fn the_stored_copy_follows_store_order() {
        let (store, _state) = store();
        let slot = Arc::new(crate::InMemoryKeySlot::default());
        let dir = tempfile::tempdir().unwrap();
        store.set_persistence(crate::persist::Persistence::new(
            slot.clone(),
            "https://h",
            dir.path().to_path_buf(),
        ));
        let gen = store.reserve_login().await;
        let held = store.cell.write().await;
        let s = store.clone();
        let install = tokio::spawn(async move { s.install_for_login(gen, session(1)).await });
        tokio::task::yield_now().await;
        let s = store.clone();
        let out = tokio::spawn(async move { s.sign_out(false).await });
        tokio::task::yield_now().await;
        drop(held);
        assert!(install.await.unwrap());
        out.await.unwrap();
        assert!(
            !slot.contains("session:https://h"),
            "the install's write landed after the sign-out's clear"
        );
    }

    /// The drop fence is synchronous: even with no runtime to run the cleanup on (and before
    /// any cleanup task runs), nothing commits into a closed store.
    #[tokio::test]
    async fn the_close_fence_holds_before_any_cleanup_runs() {
        let (store, _) = store();
        store.replace(Some(session(1))).await;
        store.close_detached(); // no runtime captured: the cleanup task never exists
        assert_eq!(
            store
                .commit_refresh("refresh-1", "access-2".into(), "refresh-2".into())
                .await,
            RefreshApplied::Discarded
        );
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
