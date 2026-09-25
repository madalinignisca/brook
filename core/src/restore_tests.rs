//! Staying signed in (plan docs/superpowers/specs/2026-09-25-keyslot-session-plan.md P2): the
//! stored session mirrors the live one, write-through, and sign-out always makes it unusable.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::client::Refresher;
use crate::test_support::{RefreshMode, TestServer};
use crate::{
    AuthState, BrookClient, CoreConfig, InMemoryKeySlot, KeySlot, KeySlotError, LoginOutcome,
    RestoreOutcome,
};

async fn strict() -> TestServer {
    let server = TestServer::start().await;
    server.set_refresh_mode(RefreshMode::Strict);
    server
}

fn client(server: &TestServer, slot: &Arc<InMemoryKeySlot>, dir: &Path) -> Arc<BrookClient> {
    let config = CoreConfig::new(&server.base).unwrap();
    let client = BrookClient::new(config).unwrap();
    client.enable_persistence(slot.clone(), dir.to_path_buf());
    Arc::new(client)
}

async fn signed_in(
    server: &TestServer,
    slot: &Arc<InMemoryKeySlot>,
    dir: &Path,
    handle: &str,
) -> Arc<BrookClient> {
    let c = client(server, slot, dir);
    assert!(matches!(
        c.login(handle, "pw").await.unwrap(),
        LoginOutcome::LoggedIn(_)
    ));
    c
}

/// A new process: a fresh client over the same slot and directory.
async fn relaunch(
    server: &TestServer,
    slot: &Arc<InMemoryKeySlot>,
    dir: &Path,
) -> (Arc<BrookClient>, RestoreOutcome) {
    let c = client(server, slot, dir);
    let outcome = c.restore().await;
    (c, outcome)
}

fn stored_token(slot: &InMemoryKeySlot, server: &TestServer) -> Option<String> {
    let name = format!("session:{}", server.base.trim_end_matches('/'));
    slot.load(name).unwrap().map(|b| {
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        v["refresh_token"].as_str().unwrap().to_string()
    })
}

async fn held_token(c: &BrookClient) -> String {
    c.session.snapshot().await.1.unwrap().refresh_token
}

#[tokio::test]
async fn a_quit_and_relaunch_stays_signed_in() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let first = signed_in(&server, &slot, dir.path(), "alice").await;
    drop(first); // quit: not a sign-out
    let (again, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert!(
        matches!(outcome, RestoreOutcome::LoggedIn(_)),
        "{outcome:?}"
    );
    assert!(matches!(*again.state().borrow(), AuthState::LoggedIn(_)));
    assert_eq!(
        stored_token(&slot, &server),
        Some(held_token(&again).await),
        "stored copy lags"
    );
}

/// Every rotation is written through, so the stored token is always the live one (the old one
/// is refused by the strict server).
#[tokio::test]
async fn the_stored_token_follows_every_rotation() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    let seen = c.session.snapshot().await.0;
    let r = Refresher {
        http: c.http.clone(),
        base: c.base.clone(),
        session: c.session.clone(),
    };
    r.refresh(seen).await.unwrap();
    assert_eq!(stored_token(&slot, &server), Some(held_token(&c).await));
    c.change_password("old-pass-1", "new-pass-2", true)
        .await
        .unwrap();
    assert_eq!(stored_token(&slot, &server), Some(held_token(&c).await));
    drop(c);
    let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert!(
        matches!(outcome, RestoreOutcome::LoggedIn(_)),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn sign_out_clears_before_it_returns() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    c.logout().await;
    assert_eq!(
        stored_token(&slot, &server),
        None,
        "stored session survived sign-out"
    );
    assert!(c.sign_out_complete());
    let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert!(
        matches!(outcome, RestoreOutcome::NotSignedIn),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn a_remote_sign_out_clears_it() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    server.set_refresh_mode(RefreshMode::Fail(401));
    let seen = c.session.snapshot().await.0;
    let r = Refresher {
        http: c.http.clone(),
        base: c.base.clone(),
        session: c.session.clone(),
    };
    let _ = r.refresh(seen).await;
    assert!(matches!(*c.state().borrow(), AuthState::LoggedOut));
    assert_eq!(stored_token(&slot, &server), None);
}

/// The keychain refuses the delete: the fence makes the still-valid stored token unusable.
#[tokio::test]
async fn a_failed_delete_is_fenced() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    server.set_stall_logout(true); // and the server never hears the sign-out either
    slot.fail_next("delete", KeySlotError::Unavailable);
    c.logout().await;
    assert!(
        stored_token(&slot, &server).is_some(),
        "(the delete failed, as scripted)"
    );
    assert!(c.sign_out_complete(), "the fence covers it");
    drop(c);
    let (again, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert!(
        matches!(outcome, RestoreOutcome::NotSignedIn),
        "restored after sign-out: {outcome:?}"
    );
    assert!(again.session.snapshot().await.1.is_none());
}

/// Delete and fence both fail: sign-out still completes locally and says it's incomplete.
#[tokio::test]
async fn a_failed_delete_and_fence_is_reported() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    slot.fail_next("delete", KeySlotError::Unavailable);
    make_unwritable(dir.path());
    c.logout().await;
    restore_writable(dir.path());
    assert!(matches!(*c.state().borrow(), AuthState::LoggedOut));
    assert!(
        !c.sign_out_complete(),
        "an incomplete sign-out was reported as complete"
    );
}

/// A fence directory that can't be read counts as a fence (fail closed).
#[tokio::test]
async fn an_unreadable_fence_refuses_restore() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    drop(signed_in(&server, &slot, dir.path(), "alice").await);
    std::fs::create_dir_all(dir.path().join("signed-out")).unwrap();
    make_unreadable(&dir.path().join("signed-out"));
    let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
    restore_writable(&dir.path().join("signed-out"));
    assert!(
        matches!(outcome, RestoreOutcome::NotSignedIn),
        "{outcome:?}"
    );
}

/// After a fenced sign-out, a new sign-in removes the fence (only once its own write worked).
#[tokio::test]
async fn a_new_sign_in_lifts_the_fence() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    slot.fail_next("delete", KeySlotError::Unavailable);
    c.logout().await;
    slot.fail_next("replace", KeySlotError::Unavailable); // the next sign-in's write fails
    assert!(matches!(
        c.login("alice", "pw").await.unwrap(),
        LoginOutcome::LoggedIn(_)
    ));
    drop(c);
    let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert!(
        matches!(outcome, RestoreOutcome::NotSignedIn),
        "fence lifted before a good write"
    );
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    drop(c);
    let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert!(
        matches!(outcome, RestoreOutcome::LoggedIn(_)),
        "fence never lifted: {outcome:?}"
    );
}

/// Switching accounts with the write failing must not leave the previous user restorable.
#[tokio::test]
async fn a_failed_write_on_a_user_switch_never_restores_the_old_user() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    server.set_stall_logout(true); // the server never hears alice's session revoked: still valid
    slot.fail_next("replace", KeySlotError::Unavailable);
    slot.fail_next("delete", KeySlotError::Unavailable);
    assert!(matches!(
        c.login("bob", "pw").await.unwrap(),
        LoginOutcome::LoggedIn(_)
    ));
    drop(c);
    let (again, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert!(
        !matches!(outcome, RestoreOutcome::LoggedIn(ref u) if u.handle == "alice"),
        "{outcome:?}"
    );
    assert!(again
        .session
        .snapshot()
        .await
        .1
        .map(|s| s.user.handle != "alice")
        .unwrap_or(true));
}

/// A restore whose refresh is rejected deletes the slot only if it still holds that token.
#[tokio::test]
async fn a_rejected_restore_deletes_only_its_own_token() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    drop(signed_in(&server, &slot, dir.path(), "alice").await);
    let gate = server.gate_refresh_rejection();
    server.set_refresh_mode(RefreshMode::Fail(401));
    let c = client(&server, &slot, dir.path());
    let c2 = c.clone();
    let restore = tokio::spawn(async move { c2.restore().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Meanwhile another process signed in as bob and stored its session.
    let other = client(&server, &slot, dir.path());
    server.set_refresh_mode(RefreshMode::Strict);
    other.login("bob", "pw").await.unwrap();
    let bobs = stored_token(&slot, &server);
    gate.add_permits(1);
    let _ = restore.await.unwrap();
    assert_eq!(
        stored_token(&slot, &server),
        bobs,
        "a stale rejection deleted a newer session"
    );
}

/// A restore that loses to a sign-out writes and installs nothing.
#[tokio::test]
async fn a_restore_losing_to_logout_leaves_nothing() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    drop(signed_in(&server, &slot, dir.path(), "alice").await);
    let gate = server.gate_refresh();
    let c = client(&server, &slot, dir.path());
    let c2 = c.clone();
    let restore = tokio::spawn(async move { c2.restore().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    c.logout().await;
    gate.add_permits(1);
    let outcome = restore.await.unwrap();
    assert!(
        !matches!(outcome, RestoreOutcome::LoggedIn(_)),
        "{outcome:?}"
    );
    assert!(c.session.snapshot().await.1.is_none());
    assert_eq!(
        stored_token(&slot, &server),
        None,
        "the losing restore wrote its pair"
    );
}

#[tokio::test]
async fn an_unreadable_slot_deletes_nothing() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    drop(signed_in(&server, &slot, dir.path(), "alice").await);
    let before = stored_token(&slot, &server);
    slot.fail_next("load", KeySlotError::Unavailable);
    let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert!(
        matches!(outcome, RestoreOutcome::Unavailable),
        "{outcome:?}"
    );
    assert_eq!(stored_token(&slot, &server), before);
}

/// Quit is not sign-out: dropping the client keeps the stored session and doesn't revoke it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quit_neither_clears_nor_revokes() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    let token = held_token(&c).await;
    drop(c);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !server.logouts().contains(&token),
        "quitting revoked the session"
    );
    assert_eq!(stored_token(&slot, &server), Some(token));
}

/// The refresh token never shows in the stored-session type's `Debug`.
#[tokio::test]
async fn the_stored_session_never_prints_its_token() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    let token = held_token(&c).await;
    drop(c);
    let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert!(!format!("{outcome:?}").contains(&token));
}

#[cfg(unix)]
fn make_unwritable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o500)).unwrap();
}
#[cfg(unix)]
fn make_unreadable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o000)).unwrap();
}
#[cfg(unix)]
fn restore_writable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
}

// ---- Several clients in one process share the slot (implementation review round 1) ----

fn refresher(c: &BrookClient) -> Refresher {
    Refresher {
        http: c.http.clone(),
        base: c.base.clone(),
        session: c.session.clone(),
    }
}

async fn refresh_now(c: &BrookClient) {
    let seen = c.session.snapshot().await.0;
    let _ = refresher(c).refresh(seen).await;
}

/// An older client's rotation after a newer sign-in in another client never overwrites it.
#[tokio::test]
async fn an_older_clients_rotation_never_overwrites_a_newer_sign_in() {
    let server = TestServer::start().await; // Rotate: any token rotates
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let alice = signed_in(&server, &slot, dir.path(), "alice").await;
    let bob = signed_in(&server, &slot, dir.path(), "bob").await;
    let bobs = held_token(&bob).await;
    refresh_now(&alice).await;
    assert_ne!(held_token(&alice).await, bobs);
    assert_eq!(
        stored_token(&slot, &server),
        Some(bobs),
        "alice's rotation overwrote bob"
    );
}

/// An older client's rejection after a newer sign-in never deletes the newer one.
#[tokio::test]
async fn an_older_clients_rejection_never_deletes_a_newer_sign_in() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let alice = signed_in(&server, &slot, dir.path(), "alice").await;
    let bob = signed_in(&server, &slot, dir.path(), "bob").await;
    let bobs = held_token(&bob).await;
    server.set_refresh_mode(RefreshMode::Fail(401));
    refresh_now(&alice).await;
    assert!(matches!(*alice.state().borrow(), AuthState::LoggedOut));
    assert_eq!(
        stored_token(&slot, &server),
        Some(bobs),
        "alice's rejection deleted bob"
    );
}

/// A restore that started before another client's sign-in and sign-out never brings the old
/// session back.
#[tokio::test]
async fn a_late_restore_never_resurrects_a_session_signed_out_elsewhere() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    drop(signed_in(&server, &slot, dir.path(), "alice").await);
    let gate = server.gate_refresh();
    let old = client(&server, &slot, dir.path());
    let old2 = old.clone();
    let restore = tokio::spawn(async move { old2.restore().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let bob = client(&server, &slot, dir.path());
    let login = tokio::spawn({
        let bob = bob.clone();
        async move { bob.login("bob", "pw").await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Bob's login is a refresh-free path; only alice's restore waits on the gate.
    assert!(matches!(
        login.await.unwrap().unwrap(),
        LoginOutcome::LoggedIn(_)
    ));
    bob.logout().await;
    gate.add_permits(1);
    let outcome = restore.await.unwrap();
    assert!(
        !matches!(outcome, RestoreOutcome::LoggedIn(_)),
        "{outcome:?}"
    );
    assert_eq!(
        stored_token(&slot, &server),
        None,
        "the late restore stored alice again"
    );
}

/// An older client's rotation never lifts the fence of a newer sign-in's sign-out.
#[tokio::test]
async fn an_older_clients_rotation_never_lifts_a_newer_fence() {
    let server = TestServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let alice = signed_in(&server, &slot, dir.path(), "alice").await;
    let bob = signed_in(&server, &slot, dir.path(), "bob").await;
    slot.fail_next("delete", KeySlotError::Unavailable);
    bob.logout().await; // the delete fails: fenced
    assert!(bob.sign_out_complete());
    refresh_now(&alice).await;
    let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert!(
        matches!(outcome, RestoreOutcome::NotSignedIn),
        "{outcome:?}"
    );
}

/// Quitting while a refresh is in flight: the server already rotated, so the stored copy
/// follows the new token, which is not revoked, and the next launch is signed in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quit_during_a_refresh_keeps_the_rotated_session() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    let gate = server.gate_refresh();
    let r = refresher(&c);
    let seen = c.session.snapshot().await.0;
    let inflight = tokio::spawn(async move { r.refresh(seen).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(c); // quit while the rotated pair is on its way
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.add_permits(1);
    let _ = inflight.await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(server.logouts().is_empty(), "the rotated token was revoked");
    gate.add_permits(1); // the relaunch's own refresh
    let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert!(
        matches!(outcome, RestoreOutcome::LoggedIn(_)),
        "{outcome:?}"
    );
}

/// A failed refresh's body can echo the token: the error (which the refresh loop logs) never
/// carries it.
#[tokio::test]
async fn a_refresh_error_never_carries_the_response_body() {
    let server = TestServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    let token = held_token(&c).await;
    server.set_refresh_mode(RefreshMode::EchoFail);
    let seen = c.session.snapshot().await.0;
    let err = refresher(&c).refresh(seen).await.unwrap_err();
    assert!(!err.to_string().contains(&token), "{err}");
    assert!(!format!("{err:?}").contains(&token), "{err:?}");
}

// ---- Round 2: only the newest client touches the slot ----

/// An older client's failed new login neither deletes the newer sign-in nor stops it from
/// following its rotations.
#[tokio::test]
async fn an_older_clients_failed_login_leaves_the_newer_sign_in_alone() {
    let server = TestServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let alice = signed_in(&server, &slot, dir.path(), "alice").await;
    let bob = signed_in(&server, &slot, dir.path(), "bob").await;
    server.set_login_fails(true);
    assert!(alice.login("alice", "pw").await.is_err());
    assert_eq!(stored_token(&slot, &server), Some(held_token(&bob).await));
    refresh_now(&bob).await;
    assert_eq!(
        stored_token(&slot, &server),
        Some(held_token(&bob).await),
        "bob's rotations stopped being stored"
    );
}

/// An older client's restore never reads (or refreshes, consuming it) a newer client's token.
#[tokio::test]
async fn an_older_clients_restore_never_touches_a_newer_sign_in() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    drop(signed_in(&server, &slot, dir.path(), "alice").await);
    let old = client(&server, &slot, dir.path());
    let bob = signed_in(&server, &slot, dir.path(), "bob").await;
    let bobs = held_token(&bob).await;
    let outcome = old.restore().await;
    assert!(matches!(outcome, RestoreOutcome::Superseded), "{outcome:?}");
    assert_eq!(stored_token(&slot, &server), Some(bobs.clone()));
    assert!(
        server.live_refresh_tokens("bob").contains(&bobs),
        "the old restore consumed bob's token"
    );
}

/// An older client that saw a fence never deletes what a newer sign-in stored meanwhile.
#[tokio::test]
async fn an_older_clients_fence_cleanup_never_deletes_a_newer_sign_in() {
    let server = TestServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let alice = signed_in(&server, &slot, dir.path(), "alice").await;
    slot.fail_next("delete", KeySlotError::Unavailable);
    alice.logout().await; // fenced
    let old = client(&server, &slot, dir.path());
    let bob = signed_in(&server, &slot, dir.path(), "bob").await; // lifts the fence
    let _ = old.restore().await;
    assert_eq!(stored_token(&slot, &server), Some(held_token(&bob).await));
}

/// A sign-out, then a quit while a refresh is in flight: the rotated token is revoked and
/// never stored (the stored-copy follow is for a quit while signed in only).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quit_after_a_sign_out_never_keeps_the_rotated_token() {
    let server = TestServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    let old = held_token(&c).await;
    let gate = server.gate_refresh();
    let r = refresher(&c);
    let seen = c.session.snapshot().await.0;
    let inflight = tokio::spawn(async move { r.refresh(seen).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    slot.fail_next("delete", KeySlotError::Unavailable);
    c.logout().await; // the delete fails: the old token stays in the slot, fenced
    drop(c);
    gate.add_permits(1);
    let _ = inflight.await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        stored_token(&slot, &server),
        Some(old.clone()),
        "the rotation was stored after sign-out"
    );
    let revoked = server.logouts();
    assert!(
        revoked.iter().any(|t| *t != old),
        "the rotated token was never revoked: {} logouts",
        revoked.len()
    );
}

/// Only 401 and 422 are the api refusing the token. Any other 4xx is something in front of it
/// (a proxy, a captive portal): the stored copy and the live session are kept.
#[tokio::test]
async fn a_4xx_not_from_the_api_never_signs_out() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    let token = held_token(&c).await;
    for status in [404, 405, 408, 413] {
        server.set_refresh_mode(RefreshMode::Fail(status));
        refresh_now(&c).await;
        assert!(
            matches!(*c.state().borrow(), AuthState::LoggedIn(_)),
            "{status} signed out"
        );
        assert_eq!(
            stored_token(&slot, &server),
            Some(token.clone()),
            "{status}"
        );
    }
    drop(c);
    for status in [404, 405, 408, 413] {
        server.set_refresh_mode(RefreshMode::Fail(status));
        let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
        assert!(
            matches!(outcome, RestoreOutcome::Offline),
            "{status}: {outcome:?}"
        );
        assert_eq!(
            stored_token(&slot, &server),
            Some(token.clone()),
            "{status}"
        );
    }
    server.set_refresh_mode(RefreshMode::Fail(422));
    let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert!(
        matches!(outcome, RestoreOutcome::NotSignedIn),
        "{outcome:?}"
    );
    assert_eq!(stored_token(&slot, &server), None);
}
