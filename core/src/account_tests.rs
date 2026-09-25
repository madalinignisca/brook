//! Password change and admin reset against the in-process origin, with strict refresh tokens
//! (a revoked one is rejected, as on the real server), so the race these guard against can
//! actually be lost.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::client::Refresher;
use crate::test_support::{PasswordMode, RefreshMode, TestServer};
use crate::{AuthState, BrookClient, CoreConfig, Error};

const WAIT: Duration = Duration::from_secs(5);

async fn signed_in(server: &TestServer, handle: &str) -> Arc<BrookClient> {
    signed_in_with(server, handle, Duration::from_secs(30)).await
}

async fn signed_in_with(server: &TestServer, handle: &str, timeout: Duration) -> Arc<BrookClient> {
    let config = CoreConfig::new(&server.base)
        .unwrap()
        .with_request_timeout(timeout);
    let client = Arc::new(BrookClient::new(config).unwrap());
    client.login(handle, "pw").await.unwrap();
    client
}

async fn strict() -> TestServer {
    let server = TestServer::start().await;
    server.set_refresh_mode(RefreshMode::Strict);
    server
}

async fn refresh_token(client: &BrookClient) -> String {
    client
        .session
        .with_session(|s| s.refresh_token.clone())
        .await
        .expect("signed in")
}

fn password_requests(server: &TestServer, path: &str) -> Vec<(String, serde_json::Value)> {
    server
        .requests()
        .into_iter()
        .filter(|(p, _, _)| p == path)
        .map(|(_, token, body)| (token, body))
        .collect()
}

async fn eventually(what: &str, f: impl Fn() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never: {what}");
}

fn secret_free(err: &Error, secrets: &[&str]) {
    let shown = format!("{err} / {err:?}");
    for secret in secrets {
        assert!(
            !shown.contains(secret),
            "a password reached the error output: {shown}"
        );
    }
}

// ---- change_password ----

#[tokio::test]
async fn change_commits_the_fresh_pair_and_revokes_the_old_one() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let old = refresh_token(&client).await;
    let rev_before = client.session.snapshot().await.0;

    client
        .change_password("old-pass-1", "new-pass-2", true)
        .await
        .unwrap();

    let new = refresh_token(&client).await;
    assert_ne!(new, old);
    assert!(
        !server.refresh_token_live(&old),
        "old refresh token still live"
    );
    assert!(
        server.refresh_token_live(&new),
        "installed token is not the server's fresh one"
    );
    let rev_after = client.session.snapshot().await.0;
    assert_eq!(
        rev_after.epoch, rev_before.epoch,
        "a password change is not a new sign-in"
    );
    assert!(rev_after.credential_rev > rev_before.credential_rev);
    // Exactly the passwords asked for, in the right fields.
    let sent = password_requests(&server, "/auth/password");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].1,
        json!({ "current_password": "old-pass-1", "new_password": "new-pass-2",
                "sign_out_other_devices": true })
    );
    // And the session keeps working: a refresh with the new token succeeds.
    let refresher = Refresher {
        http: client.http.clone(),
        base: client.base.clone(),
        session: client.session.clone(),
    };
    let rev = client.session.snapshot().await.0;
    refresher.refresh(rev).await.unwrap();
    assert!(matches!(*client.state().borrow(), AuthState::LoggedIn(_)));
}

/// Unchecked, the flag is sent as false: never left out for a server default to decide.
#[tokio::test]
async fn keeping_other_devices_signed_in_is_sent_explicitly() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    client
        .change_password("old-pass-1", "new-pass-2", false)
        .await
        .unwrap();
    let sent = password_requests(&server, "/auth/password");
    assert_eq!(
        sent[0].1,
        json!({ "current_password": "old-pass-1", "new_password": "new-pass-2",
                "sign_out_other_devices": false })
    );
}

/// The race: a background refresh (the production single-flight path) arrives while the change
/// is in flight. It must wait, see the new credentials, and send nothing: sending the old,
/// now-revoked token would be rejected and sign the user out by their own password change.
#[tokio::test]
async fn a_refresh_racing_the_change_never_signs_the_user_out() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let gate = server.gate_password();
    let seen = client.session.snapshot().await.0;

    let c = client.clone();
    let change =
        tokio::spawn(async move { c.change_password("old-pass-1", "new-pass-2", true).await });
    eventually("server committed the change", || {
        !password_requests(&server, "/auth/password").is_empty()
    })
    .await;
    let refresher = Refresher {
        http: client.http.clone(),
        base: client.base.clone(),
        session: client.session.clone(),
    };
    let refresh = tokio::spawn(async move { refresher.refresh(seen).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !refresh.is_finished(),
        "the refresh did not wait for the change"
    );
    gate.add_permits(1);

    tokio::time::timeout(WAIT, change)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(WAIT, refresh)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        server.refresh_calls(),
        0,
        "the stale refresh reached the server"
    );
    assert!(
        matches!(*client.state().borrow(), AuthState::LoggedIn(_)),
        "signed out"
    );
    assert!(server.refresh_token_live(&refresh_token(&client).await));
}

/// A refresh that completed just before the change: the change uses the rotated credentials.
#[tokio::test]
async fn a_refresh_just_before_the_change_is_harmless() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let refresher = Refresher {
        http: client.http.clone(),
        base: client.base.clone(),
        session: client.session.clone(),
    };
    let rev = client.session.snapshot().await.0;
    refresher.refresh(rev).await.unwrap();
    let rotated = client.session.snapshot().await.1.unwrap().access_token;
    client
        .change_password("old-pass-1", "new-pass-2", true)
        .await
        .unwrap();
    assert_eq!(password_requests(&server, "/auth/password")[0].0, rotated);
    assert!(server.refresh_token_live(&refresh_token(&client).await));
}

#[tokio::test]
async fn expired_access_refreshes_once_and_retries_once() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    server.expire_next(1);
    client
        .change_password("old-pass-1", "new-pass-2", true)
        .await
        .unwrap();
    assert_eq!(server.refresh_calls(), 1);
    let sent = password_requests(&server, "/auth/password");
    assert_eq!(sent.len(), 2);
    assert_ne!(
        sent[0].0, sent[1].0,
        "the retry did not use the refreshed access token"
    );
    // The pair from the retry is installed (CAS against the rotated token, not the start one).
    assert!(server.refresh_token_live(&refresh_token(&client).await));
    assert!(matches!(*client.state().borrow(), AuthState::LoggedIn(_)));
}

#[tokio::test]
async fn repeated_401_does_not_loop() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    server.expire_next(10);
    let err = client
        .change_password("old-pass-1", "new-pass-2", true)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotAuthenticated), "{err:?}");
    assert_eq!(server.refresh_calls(), 1);
    assert_eq!(password_requests(&server, "/auth/password").len(), 2);
}

#[tokio::test]
async fn wrong_current_password_never_refreshes() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let old = refresh_token(&client).await;
    server.set_password_mode(PasswordMode::WrongCurrent);
    let err = client
        .change_password("wrong-pass", "new-pass-2", true)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Api { code, .. } if code == "auth.invalid_credentials"),
        "{err:?}"
    );
    assert_eq!(server.refresh_calls(), 0);
    assert_eq!(refresh_token(&client).await, old);
    assert!(server.refresh_token_live(&old));
}

#[tokio::test]
async fn a_refused_new_password_never_reaches_the_error_text() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    server.set_password_mode(PasswordMode::Echo422);
    let err = client
        .change_password("Sekret-Old-1", "Sekret-New-2", true)
        .await
        .unwrap_err();
    secret_free(&err, &["Sekret-Old-1", "Sekret-New-2"]);
}

#[tokio::test]
async fn signed_out_sends_nothing() {
    let server = strict().await;
    let client = server.client();
    let err = client
        .change_password("old-pass-1", "new-pass-2", true)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotAuthenticated));
    assert!(server.requests().is_empty());
}

/// Login takes the same lock: it cannot install a pair the change is about to revoke.
#[tokio::test]
async fn a_login_during_the_change_waits_for_it() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let gate = server.gate_password();
    let c = client.clone();
    let change =
        tokio::spawn(async move { c.change_password("old-pass-1", "new-pass-2", true).await });
    eventually("server committed the change", || {
        !password_requests(&server, "/auth/password").is_empty()
    })
    .await;
    let c = client.clone();
    let login = tokio::spawn(async move { c.login("alice", "new-pass-2").await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!login.is_finished(), "login did not wait for the change");
    gate.add_permits(1);
    tokio::time::timeout(WAIT, change)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(WAIT, login)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        server.refresh_token_live(&refresh_token(&client).await),
        "login installed a revoked pair"
    );
}

/// The caller goes away after the server committed: the fresh pair is still installed.
#[tokio::test]
async fn cancelling_after_the_server_committed_still_installs_the_pair() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let gate = server.gate_password();
    let c = client.clone();
    let caller =
        tokio::spawn(async move { c.change_password("old-pass-1", "new-pass-2", true).await });
    eventually("server committed the change", || {
        !password_requests(&server, "/auth/password").is_empty()
    })
    .await;
    caller.abort();
    gate.add_permits(1);
    let c = client.clone();
    let srv_live = move |t: &str| server.refresh_token_live(t);
    for _ in 0..500 {
        if srv_live(&refresh_token(&c).await) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the committed pair was never installed");
}

/// The locked section is bounded inside the worker: when the bound fires the lock is released
/// (a queued login proceeds) and a late response cannot commit afterwards.
#[tokio::test]
async fn the_bound_releases_the_lock_and_nothing_commits_late() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let mut shortened = Arc::try_unwrap(client).ok().expect("sole owner");
    shortened.locked_bound = Duration::from_millis(300);
    let client = Arc::new(shortened);
    let gate = server.gate_password();

    let err = tokio::time::timeout(
        WAIT,
        client.change_password("old-pass-1", "new-pass-2", true),
    )
    .await
    .expect("change not bounded")
    .unwrap_err();
    assert!(matches!(err, Error::Timeout), "{err:?}");
    tokio::time::timeout(WAIT, client.login("alice", "new-pass-2"))
        .await
        .expect("the lock was not released")
        .unwrap();
    let after_login = refresh_token(&client).await;
    gate.add_permits(1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        refresh_token(&client).await,
        after_login,
        "a late response committed"
    );
}

/// No request can hold the refresh lock forever: a stalled refresh gives way within the
/// client's request bound, and a login waiting behind it goes through.
#[tokio::test]
async fn a_stalled_refresh_does_not_block_login_forever() {
    let server = strict().await;
    let client = signed_in_with(&server, "alice", Duration::from_millis(300)).await;
    server.set_refresh_mode(RefreshMode::Stall);
    let refresher = Refresher {
        http: client.http.clone(),
        base: client.base.clone(),
        session: client.session.clone(),
    };
    let rev = client.session.snapshot().await.0;
    let _stalled = tokio::spawn(async move { refresher.refresh(rev).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.set_refresh_mode(RefreshMode::Strict);
    tokio::time::timeout(WAIT, client.login("alice", "pw"))
        .await
        .expect("login blocked behind a stalled refresh")
        .unwrap();
}

// ---- admin ----

#[tokio::test]
async fn admin_reset_sends_the_new_password_and_revokes_only_the_target() {
    let server = strict().await;
    let admin = signed_in(&server, "admin").await;
    let bob = signed_in(&server, "bob").await;
    let bob_token = refresh_token(&bob).await;
    let admin_token = refresh_token(&admin).await;

    admin
        .admin_reset_password("id-bob", "admin-pw", "bobs-new-pass")
        .await
        .unwrap();

    assert!(
        !server.refresh_token_live(&bob_token),
        "target not signed out"
    );
    assert!(
        server.refresh_token_live(&admin_token),
        "the admin's own session was revoked"
    );
    assert_eq!(
        refresh_token(&admin).await,
        admin_token,
        "the admin's session changed"
    );
    let sent = password_requests(&server, "/users/id-bob/password");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].1,
        json!({ "admin_password": "admin-pw", "new_password": "bobs-new-pass" })
    );
}

#[tokio::test]
async fn admin_reset_of_oneself_is_refused_locally() {
    let server = strict().await;
    let admin = signed_in(&server, "admin").await;
    let err = admin
        .admin_reset_password("id-admin", "admin-pw", "whatever-123")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Api { code, .. } if code == "invalid"),
        "{err:?}"
    );
    assert!(password_requests(&server, "/users/id-admin/password").is_empty());
}

#[tokio::test]
async fn admin_reset_errors_are_mapped_and_secret_free() {
    let server = strict().await;
    let admin = signed_in(&server, "admin").await;
    for (mode, code) in [
        (PasswordMode::WrongCurrent, "auth.invalid_credentials"),
        (PasswordMode::Forbidden, "authz.forbidden"),
        (PasswordMode::SelfTarget, "invalid"),
        (PasswordMode::NotFound, "not_found"),
    ] {
        server.set_password_mode(mode);
        let err = admin
            .admin_reset_password("id-bob", "admin-pw", "bobs-new-pass")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Api { code: c, .. } if c == code),
            "{mode:?}: {err:?}"
        );
    }
    server.set_password_mode(PasswordMode::Echo422);
    let err = admin
        .admin_reset_password("id-bob", "admin-pw", "Sekret-New-3")
        .await
        .unwrap_err();
    secret_free(&err, &["Sekret-New-3", "admin-pw"]);
}

#[tokio::test]
async fn admin_calls_refresh_once_on_401_and_do_not_loop() {
    let server = strict().await;
    let admin = signed_in(&server, "admin").await;
    server.expire_next(1);
    admin
        .admin_reset_password("id-bob", "admin-pw", "bobs-new-pass")
        .await
        .unwrap();
    assert_eq!(server.refresh_calls(), 1);
    assert_eq!(
        password_requests(&server, "/users/id-bob/password").len(),
        2
    );

    server.expire_next(1);
    admin.list_users().await.unwrap();
    assert_eq!(server.refresh_calls(), 2);

    server.expire_next(10);
    assert!(matches!(
        admin.list_users().await.unwrap_err(),
        Error::NotAuthenticated
    ));
    assert!(matches!(
        admin
            .admin_reset_password("id-bob", "admin-pw", "bobs-new-pass")
            .await
            .unwrap_err(),
        Error::NotAuthenticated
    ));
    assert_eq!(server.refresh_calls(), 4, "more than one refresh per call");
}

#[tokio::test]
async fn list_users_parses_and_maps_forbidden() {
    let server = strict().await;
    let admin = signed_in(&server, "admin").await;
    let _bob = signed_in(&server, "bob").await;
    let users = admin.list_users().await.unwrap();
    assert_eq!(
        users.iter().map(|u| u.handle.as_str()).collect::<Vec<_>>(),
        ["admin", "bob"]
    );
    assert_eq!(users[1].id, "id-bob");
    server.set_password_mode(PasswordMode::Forbidden);
    let err = admin.list_users().await.unwrap_err();
    assert!(matches!(&err, Error::Api { code, .. } if code == "authz.forbidden"));
}

/// An id is one path segment: it cannot address another route.
#[tokio::test]
async fn a_user_id_cannot_escape_its_path_segment() {
    let server = strict().await;
    let admin = signed_in(&server, "admin").await;
    let _ = admin
        .admin_reset_password("../auth", "admin-pw", "whatever-123")
        .await;
    assert!(
        password_requests(&server, "/auth/password").is_empty(),
        "the id escaped its segment"
    );
}

/// A sign-in that lands between reading the session and getting the lock (here: another user's
/// session installed while the lock is held elsewhere): the change must not go out with the new
/// session's token.
#[tokio::test]
async fn a_change_is_bound_to_the_session_it_started_in() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let held = client.session.refresh_lock.clone().lock_owned().await;
    let c = client.clone();
    let change =
        tokio::spawn(async move { c.change_password("old-pass-1", "new-pass-2", true).await });
    tokio::time::sleep(Duration::from_millis(100)).await; // it read the epoch and waits
    let bob = signed_in(&server, "bob").await;
    let bobs = bob.session.snapshot().await.1.unwrap();
    client.session.replace(Some(bobs)).await; // a new epoch, another identity
    drop(held);
    let err = tokio::time::timeout(WAIT, change)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, Error::NotAuthenticated), "{err:?}");
    assert!(
        password_requests(&server, "/auth/password").is_empty(),
        "sent with another session's token"
    );
}

/// The admin calls refresh through the single-flight path: with a refresh already holding the
/// lock, their 401 path waits instead of sending the same token a second time (whose rejection
/// would sign the admin out).
#[tokio::test]
async fn admin_401_refresh_waits_for_a_refresh_in_flight() {
    let server = strict().await;
    let admin = signed_in(&server, "admin").await;
    let held = admin.session.refresh_lock.clone().lock_owned().await;
    server.expire_next(1);
    let a = admin.clone();
    let call = tokio::spawn(async move { a.list_users().await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        server.refresh_calls(),
        0,
        "refreshed without waiting for the lock"
    );
    drop(held);
    tokio::time::timeout(WAIT, call)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(server.refresh_calls(), 1);
}

/// The access token expired and the refresh is rate limited: "too many attempts", and the user
/// stays signed in (a 429 is not a rejected token).
#[tokio::test]
async fn a_rate_limited_refresh_during_a_change_keeps_the_session() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let before = refresh_token(&client).await;
    server.expire_next(1);
    server.set_refresh_mode(RefreshMode::RateLimited(30));
    let err = client
        .change_password("old-pass-1", "new-pass-2", true)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Api { code, .. } if code == "auth.rate_limited"),
        "{err:?}"
    );
    assert!(
        matches!(*client.state().borrow(), AuthState::LoggedIn(_)),
        "signed out"
    );
    assert_eq!(refresh_token(&client).await, before);
}

/// A 401 that was in flight while a background refresh committed: the refresh already happened,
/// so the retry uses the new credentials and nothing is refreshed twice (a second refresh can be
/// rate limited, turning a working session into "too many attempts").
#[tokio::test]
async fn a_401_answered_after_a_refresh_retries_without_refreshing_again() {
    let server = strict().await;
    let admin = signed_in(&server, "admin").await;
    let seen = admin.session.snapshot().await.0;
    let gate = server.gate_expired();
    server.expire_next(1);
    let a = admin.clone();
    let call = tokio::spawn(async move { a.list_users().await });
    eventually("the list request reached the server", || {
        !password_requests(&server, "/users").is_empty()
    })
    .await;
    let refresher = Refresher {
        http: admin.http.clone(),
        base: admin.base.clone(),
        session: admin.session.clone(),
    };
    refresher.refresh(seen).await.unwrap();
    assert_eq!(server.refresh_calls(), 1);
    gate.add_permits(1);
    tokio::time::timeout(WAIT, call)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(server.refresh_calls(), 1, "refreshed again after a refresh");
}

/// A 401 that was in flight while another login replaced the session: nothing of the new
/// session is refreshed on the old call's behalf.
#[tokio::test]
async fn a_401_answered_after_a_new_login_refreshes_nothing() {
    let server = strict().await;
    let admin = signed_in(&server, "admin").await;
    let gate = server.gate_expired();
    server.expire_next(1);
    let a = admin.clone();
    let call = tokio::spawn(async move { a.list_users().await });
    eventually("the list request reached the server", || {
        !password_requests(&server, "/users").is_empty()
    })
    .await;
    let bob = signed_in(&server, "bob").await;
    let bobs = bob.session.snapshot().await.1.unwrap();
    admin.session.replace(Some(bobs)).await;
    gate.add_permits(1);
    let err = tokio::time::timeout(WAIT, call)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, Error::NotAuthenticated), "{err:?}");
    assert_eq!(server.refresh_calls(), 0, "refreshed another session");
}

/// With "sign out other devices", the server closes this device's own socket (1008
/// `session_revoked`) before the password response goes out. The socket's refresh must wait for
/// the change, see the new pair and reconnect with it: sending the revoked refresh token would be
/// rejected, and a rejection signs the user out by their own password change.
#[tokio::test]
async fn own_socket_revoked_before_the_response_stays_signed_in() {
    let mut server = strict().await;
    let client = signed_in(&server, "alice").await;
    client.start_realtime().await.unwrap();
    let mut peer = server.accept().await;
    peer.accept_auth().await;
    let gate = server.gate_password();

    let c = client.clone();
    let change =
        tokio::spawn(async move { c.change_password("old-pass-1", "new-pass-2", true).await });
    eventually("server committed the change", || {
        !password_requests(&server, "/auth/password").is_empty()
    })
    .await;
    peer.close(1008, "session_revoked").await;
    tokio::time::sleep(Duration::from_millis(200)).await; // the socket's refresh is waiting
    assert_eq!(
        server.refresh_calls(),
        0,
        "refreshed while the change was in flight"
    );
    gate.add_permits(1);
    tokio::time::timeout(WAIT, change)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let mut again = server.accept().await;
    let auth = again.accept_auth().await;
    let session = client.session.snapshot().await.1.unwrap();
    assert_eq!(
        auth["data"]["access_token"], session.access_token,
        "reconnected without the new pair"
    );
    assert!(
        matches!(*client.state().borrow(), AuthState::LoggedIn(_)),
        "signed out by its own password change"
    );
    assert_eq!(server.refresh_calls(), 0, "the revoked token was sent");
}

/// The other ordering: the socket is closed after the new pair is committed. Its refresh sees the
/// new revision and reconnects with the new pair, sending nothing.
#[tokio::test]
async fn own_socket_revoked_after_the_commit_reconnects_with_the_new_pair() {
    let mut server = strict().await;
    let client = signed_in(&server, "alice").await;
    client.start_realtime().await.unwrap();
    let mut peer = server.accept().await;
    peer.accept_auth().await;
    client
        .change_password("old-pass-1", "new-pass-2", true)
        .await
        .unwrap();
    peer.close(1008, "session_revoked").await;

    let mut again = server.accept().await;
    let auth = again.accept_auth().await;
    let session = client.session.snapshot().await.1.unwrap();
    assert_eq!(auth["data"]["access_token"], session.access_token);
    assert!(matches!(*client.state().borrow(), AuthState::LoggedIn(_)));
    assert_eq!(server.refresh_calls(), 0, "the revoked token was sent");
}

/// A REST call of this device with the old access token, answered 401 while the change is in
/// flight: its refresh waits for the change and retries with the new pair.
#[tokio::test]
async fn own_rest_call_with_the_revoked_access_token_retries_with_the_new_pair() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let gate = server.gate_password();
    let c = client.clone();
    let change =
        tokio::spawn(async move { c.change_password("old-pass-1", "new-pass-2", true).await });
    eventually("server committed the change", || {
        !password_requests(&server, "/auth/password").is_empty()
    })
    .await;
    let c = client.clone();
    let list = tokio::spawn(async move { c.list_users().await });
    eventually("the list got its 401", || {
        !password_requests(&server, "/users").is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        server.refresh_calls(),
        0,
        "refreshed while the change was in flight"
    );
    gate.add_permits(1);
    tokio::time::timeout(WAIT, change)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(WAIT, list)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let sent = password_requests(&server, "/users");
    assert_eq!(sent.len(), 2);
    let session = client.session.snapshot().await.1.unwrap();
    assert_eq!(
        sent[1].0, session.access_token,
        "retried without the new pair"
    );
    assert!(matches!(*client.state().borrow(), AuthState::LoggedIn(_)));
    assert_eq!(server.refresh_calls(), 0);
}
