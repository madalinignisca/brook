//! Keeping the signed-in user current (spec docs/superpowers/specs/2026-09-26-core-stored-user-spec.md):
//! a profile change is kept beside the stored token, and a restore asks for the current
//! profile, without ever fencing, un-fencing or touching a token.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::client::Refresher;
use crate::test_support::{MeMode, RefreshMode, TestServer};
use crate::{
    AuthState, BrookClient, CoreConfig, InMemoryKeySlot, KeySlot, KeySlotError, LoginOutcome,
    RestoreOutcome, User,
};

/// Every restore here must finish well inside this, or it hung.
const HANG: Duration = Duration::from_secs(5);

async fn strict() -> TestServer {
    let server = TestServer::start().await;
    server.set_refresh_mode(RefreshMode::Strict);
    server
}

fn client(server: &TestServer, slot: &Arc<InMemoryKeySlot>, dir: &Path) -> Arc<BrookClient> {
    let config = CoreConfig::new(&server.base).unwrap();
    let mut client = BrookClient::new(config).unwrap();
    client.profile_wait = Duration::from_millis(400);
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

async fn relaunch(
    server: &TestServer,
    slot: &Arc<InMemoryKeySlot>,
    dir: &Path,
) -> (Arc<BrookClient>, RestoreOutcome) {
    let c = client(server, slot, dir);
    let outcome = tokio::time::timeout(HANG, c.restore())
        .await
        .expect("the restore hung");
    (c, outcome)
}

fn slot_name(server: &TestServer) -> String {
    format!("session:{}", server.base.trim_end_matches('/'))
}

fn stored(slot: &InMemoryKeySlot, server: &TestServer) -> Option<serde_json::Value> {
    slot.load(slot_name(server))
        .unwrap()
        .map(|b| serde_json::from_slice(&b).unwrap())
}

fn stored_name(slot: &InMemoryKeySlot, server: &TestServer) -> Option<String> {
    stored(slot, server).map(|v| v["user"]["display_name"].as_str().unwrap().to_string())
}

fn fenced(dir: &Path) -> bool {
    std::fs::read_dir(dir.join("signed-out"))
        .map(|d| {
            d.filter_map(Result::ok)
                .any(|e| !e.file_name().to_string_lossy().starts_with('.'))
        })
        .unwrap_or(false)
}

fn logged_in_name(outcome: &RestoreOutcome) -> String {
    match outcome {
        RestoreOutcome::LoggedIn(user) => user.display_name.clone(),
        other => panic!("not signed in: {other:?}"),
    }
}

async fn held(c: &BrookClient) -> String {
    c.session.snapshot().await.1.unwrap().refresh_token
}

async fn session_user(c: &BrookClient) -> User {
    c.session.snapshot().await.1.unwrap().user
}

/// This device's edit is kept beside the token: the next launch shows it even when the
/// server can't be asked (`me` failing), so it's the stored copy, not the server, answering.
#[tokio::test]
async fn a_profile_change_is_stored_for_the_next_launch() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    c.update_profile(Some("Alice A"), None).await.unwrap();
    assert_eq!(stored_name(&slot, &server).as_deref(), Some("Alice A"));
    assert_eq!(session_user(&c).await.display_name, "Alice A");
    drop(c);
    server.set_me_mode(MeMode::Fail(500));
    let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert_eq!(logged_in_name(&outcome), "Alice A");
}

/// Another device's edit reaches this one at its next launch, and is kept.
#[tokio::test]
async fn a_restore_takes_the_servers_current_profile() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    drop(signed_in(&server, &slot, dir.path(), "alice").await);
    server.set_profile("alice", "Ally");
    let (again, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert_eq!(logged_in_name(&outcome), "Ally");
    assert_eq!(stored_name(&slot, &server).as_deref(), Some("Ally"));
    assert_eq!(session_user(&again).await.display_name, "Ally");
    // Not a self-deadlock: the restore's flight has ended, so a refresh still runs.
    let seen = again.session.snapshot().await.0;
    let r = Refresher {
        http: again.http.clone(),
        base: again.base.clone(),
        session: again.session.clone(),
    };
    tokio::time::timeout(HANG, r.refresh(seen))
        .await
        .expect("a refresh after the restore hung")
        .unwrap();
}

/// When the profile can't be read in time, or answers for someone else, the stored user
/// signs in: never a failed restore, never another person's name.
#[tokio::test]
async fn a_restore_keeps_the_stored_user_when_the_profile_fails() {
    for mode in [MeMode::Fail(500), MeMode::Stall, MeMode::OtherId] {
        let server = strict().await;
        let dir = tempfile::tempdir().unwrap();
        let slot = Arc::new(InMemoryKeySlot::default());
        drop(signed_in(&server, &slot, dir.path(), "alice").await);
        server.set_profile("alice", "Ally");
        server.set_me_mode(mode);
        let started = std::time::Instant::now();
        let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
        assert_eq!(logged_in_name(&outcome), "alice", "{mode:?}");
        assert_eq!(
            stored_name(&slot, &server).as_deref(),
            Some("alice"),
            "{mode:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{mode:?}: not bounded"
        );
    }
}

#[tokio::test]
async fn replace_user_refuses_another_user_a_stale_session_and_a_closed_store() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    let epoch = c.session.snapshot().await.0.epoch;
    let mut me = session_user(&c).await;

    let mut bob = me.clone();
    bob.id = "id-bob".into();
    bob.display_name = "Bob".into();
    assert!(!c.session.replace_user(epoch, bob).await, "another user");
    assert_eq!(
        stored_name(&slot, &server).as_deref(),
        Some("alice"),
        "written anyway"
    );

    me.display_name = "Late".into();
    c.logout().await;
    assert!(
        !c.session.replace_user(epoch, me.clone()).await,
        "signed out"
    );
    assert!(matches!(
        c.login("alice", "pw").await.unwrap(),
        LoginOutcome::LoggedIn(_)
    ));
    assert!(
        !c.session.replace_user(epoch, me.clone()).await,
        "the same user signed in again: another session"
    );
    assert_eq!(stored_name(&slot, &server).as_deref(), Some("alice"));

    let now = c.session.snapshot().await.0.epoch;
    c.session.close_detached();
    assert!(!c.session.replace_user(now, me).await, "closed");
    assert_eq!(stored_name(&slot, &server).as_deref(), Some("alice"));
}

/// A fenced slot is never lifted by a profile write, even when it holds this session's token.
#[tokio::test]
async fn a_fenced_slot_stays_fenced() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    slot.fail_next("replace", KeySlotError::Unavailable); // the login's write fails: fenced
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    assert!(fenced(dir.path()));
    let session = c.session.snapshot().await.1.unwrap();
    let record =
        serde_json::json!({ "user": session.user, "refresh_token": session.refresh_token });
    slot.put(&slot_name(&server), serde_json::to_vec(&record).unwrap());
    c.update_profile(Some("Alice A"), None).await.unwrap();
    assert!(fenced(dir.path()), "the fence was lifted");
    assert_eq!(stored_name(&slot, &server).as_deref(), Some("alice"));
}

/// Only a record holding this session's token is rewritten: never another token, never one
/// that couldn't be read, never a slot cleared on purpose.
#[tokio::test]
async fn only_this_sessions_record_is_rewritten() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    let name = slot_name(&server);

    let user = session_user(&c).await;
    let foreign = serde_json::json!({ "user": user, "refresh_token": "someone-elses" });
    slot.put(&name, serde_json::to_vec(&foreign).unwrap());
    c.update_profile(Some("One"), None).await.unwrap();
    let now = stored(&slot, &server).unwrap();
    assert_eq!(now["refresh_token"], "someone-elses");
    assert_eq!(now["user"]["display_name"], "alice");

    let session = c.session.snapshot().await.1.unwrap();
    let ours = serde_json::json!({ "user": session.user, "refresh_token": session.refresh_token });
    slot.put(&name, serde_json::to_vec(&ours).unwrap());
    slot.fail_next("load", KeySlotError::Unavailable);
    c.update_profile(Some("Two"), None).await.unwrap();
    assert_eq!(
        stored_name(&slot, &server).as_deref(),
        Some("One"),
        "written after a failed read"
    );

    slot.delete(name.clone()).unwrap();
    c.update_profile(Some("Three"), None).await.unwrap();
    assert!(!slot.contains(&name), "a cleared slot came back");
}

/// A failed profile write fences nothing: the stored copy, with the older name, still restores.
#[tokio::test]
async fn a_failed_write_fences_nothing() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    slot.fail_next("replace", KeySlotError::Unavailable);
    c.update_profile(Some("Alice A"), None).await.unwrap();
    assert!(!fenced(dir.path()), "a profile write fenced the session");
    assert!(c.sign_out_complete());
    drop(c);
    server.set_me_mode(MeMode::Fail(500));
    let (_, outcome) = relaunch(&server, &slot, dir.path()).await;
    assert_eq!(logged_in_name(&outcome), "alice");
}

/// An older client never writes a newer client's slot.
#[tokio::test]
async fn an_older_client_never_writes_a_newer_ones_slot() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let old = signed_in(&server, &slot, dir.path(), "alice").await;
    let _new = client(&server, &slot, dir.path()); // takes the slot
    old.update_profile(Some("Old"), None).await.unwrap();
    assert_eq!(stored_name(&slot, &server).as_deref(), Some("alice"));
}

/// A profile change isn't a sign-in: no `AuthState`, no credential revision (no socket re-auth).
#[tokio::test]
async fn a_profile_change_publishes_nothing() {
    let server = strict().await;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let c = signed_in(&server, &slot, dir.path(), "alice").await;
    let mut state = c.state();
    state.borrow_and_update();
    let mut revs = c.session.watch();
    revs.borrow_and_update();
    let before = c.session.snapshot().await.0;
    let token = held(&c).await;
    assert!(c.session.marked_current(&token));
    let family = c.session.persistence().unwrap().family();
    c.update_profile(Some("Alice A"), None).await.unwrap();
    assert_eq!(
        c.session.persistence().unwrap().family(),
        family,
        "the stored login's generation moved"
    );
    assert!(!state.has_changed().unwrap(), "AuthState re-published");
    assert!(
        !revs.has_changed().unwrap(),
        "the revision watch woke (a socket re-auth)"
    );
    assert!(c.session.marked_current(&token), "the login's marker moved");
    assert!(matches!(&*state.borrow(), AuthState::LoggedIn(u) if u.display_name == "alice"));
    assert_eq!(c.session.snapshot().await.0, before);
}

/// A profile change and a refresh at once both finish (the lock order holds).
#[tokio::test]
async fn a_profile_change_during_a_refresh_completes() {
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
    let (refreshed, changed) = tokio::time::timeout(HANG, async {
        tokio::join!(r.refresh(seen), c.update_profile(Some("Alice A"), None))
    })
    .await
    .expect("a deadlock");
    refreshed.unwrap();
    changed.unwrap();
    let now = stored(&slot, &server).unwrap();
    assert_eq!(now["user"]["display_name"], "Alice A");
    assert_eq!(
        now["refresh_token"],
        c.session.snapshot().await.1.unwrap().refresh_token
    );
}
