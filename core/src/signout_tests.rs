//! Sign out, and no refresh token left live that no device holds (spec
//! docs/superpowers/specs/2026-09-25-sign-out-design.md §5, plan P2).

use std::sync::Arc;
use std::time::Duration;

use crate::client::{RefreshOutcome, Refresher};
use crate::test_support::{PasswordMode, RefreshMode, TestServer};
use crate::{AuthState, BrookClient, CoreConfig, Error};

const WAIT: Duration = Duration::from_secs(5);

async fn strict() -> TestServer {
    let server = TestServer::start().await;
    server.set_refresh_mode(RefreshMode::Strict);
    server
}

async fn signed_in(server: &TestServer, handle: &str) -> Arc<BrookClient> {
    let client = Arc::new(BrookClient::new(CoreConfig::new(&server.base).unwrap()).unwrap());
    client.login(handle, "pw").await.unwrap();
    client
}

async fn refresh_token(client: &BrookClient) -> String {
    client.session.snapshot().await.1.unwrap().refresh_token
}

async fn eventually(what: &str, f: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !f() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn refresher(client: &BrookClient) -> Refresher {
    Refresher {
        http: client.http.clone(),
        base: client.base.clone(),
        session: client.session.clone(),
    }
}

fn logged_out(client: &BrookClient) -> bool {
    matches!(*client.state().borrow(), AuthState::LoggedOut)
}

// ---- logout ----

#[tokio::test]
async fn logout_revokes_the_session_and_publishes_logged_out() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let token = refresh_token(&client).await;

    client.logout().await;

    assert!(logged_out(&client));
    assert!(matches!(
        client.list_channels().await,
        Err(Error::NotAuthenticated)
    ));
    eventually("the server revoked the token", || {
        server.logouts() == vec![token.clone()]
    })
    .await;
    assert!(server.live_refresh_tokens("alice").is_empty());
}

/// When `logout` returns, the server has heard it: an app quitting right after (Sign Out,
/// then Quit) doesn't leave the refresh token live.
#[tokio::test]
async fn logout_returns_once_the_server_heard_it() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let token = refresh_token(&client).await;
    client.logout().await;
    assert_eq!(
        server.logouts(),
        vec![token],
        "logout returned before the revoke went out"
    );
}

/// A server that doesn't answer delays the sign-out by the bound at most; it is local and
/// already done by then.
#[tokio::test]
async fn logout_waits_for_the_revoke_only_so_long() {
    let server = strict().await;
    let mut client = BrookClient::new(CoreConfig::new(&server.base).unwrap()).unwrap();
    client.revoke_wait = Duration::from_millis(200);
    client.login("alice", "pw").await.unwrap();
    server.set_stall_logout(true);
    let started = tokio::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(2), client.logout())
        .await
        .expect("logout waited past its bound");
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "it didn't wait at all"
    );
    assert!(logged_out(&client));
}

#[tokio::test]
async fn logout_without_a_session_sends_nothing() {
    let server = strict().await;
    let client = BrookClient::new(CoreConfig::new(&server.base).unwrap()).unwrap();
    client.logout().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(server.logouts().is_empty());
    assert!(logged_out(&client));
}

/// Sign-out never waits for the refresh lock: a refresh stalled after the server rotated
/// must not hold the user signed in. When it lands, its new token is revoked (no orphan).
#[tokio::test]
async fn logout_does_not_wait_for_a_refresh_and_its_pair_is_revoked() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let gate = server.gate_refresh();
    let seen = client.session.snapshot().await.0;
    let r = refresher(&client);
    let refresh = tokio::spawn(async move { r.refresh(seen).await });
    eventually("the server rotated", || server.refresh_calls() == 1).await;

    tokio::time::timeout(Duration::from_secs(1), client.logout())
        .await
        .expect("sign-out waited for the refresh");
    assert!(logged_out(&client));

    gate.add_permits(1);
    let outcome = tokio::time::timeout(WAIT, refresh).await.unwrap().unwrap();
    assert!(
        matches!(outcome, Ok(RefreshOutcome::Discarded)),
        "{outcome:?}"
    );
    eventually("no live refresh token left", || {
        server.live_refresh_tokens("alice").is_empty()
    })
    .await;
    assert!(logged_out(&client), "the late refresh signed back in");
}

/// A password change whose response arrives after sign-out: the pair it returns is revoked.
#[tokio::test]
async fn a_password_change_landing_after_logout_leaves_no_live_token() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let gate = server.gate_password();
    let c = client.clone();
    let change =
        tokio::spawn(async move { c.change_password("old-pass-1", "new-pass-2", true).await });
    eventually("the server committed the change", || {
        server
            .requests()
            .iter()
            .any(|(p, _, _)| p == "/auth/password")
    })
    .await;
    client.logout().await;
    gate.add_permits(1);
    let _ = tokio::time::timeout(WAIT, change).await.unwrap().unwrap();
    eventually("no live refresh token left", || {
        server.live_refresh_tokens("alice").is_empty()
    })
    .await;
    assert!(logged_out(&client));
}

// ---- login ----

/// A new login takes the old session out up front; its refresh token is revoked then,
/// whatever the new login's outcome.
#[tokio::test]
async fn a_login_revokes_the_session_it_displaces_even_when_it_fails() {
    for fails in [false, true] {
        let server = strict().await;
        let client = signed_in(&server, "alice").await;
        let old = refresh_token(&client).await;
        server.set_login_fails(fails);
        let result = client.login("alice", "pw").await;
        assert_eq!(result.is_err(), fails);
        eventually("the displaced token was revoked", || {
            server.logouts().contains(&old)
        })
        .await;
        let live = server.live_refresh_tokens("alice");
        let held = client.session.snapshot().await.1.map(|s| s.refresh_token);
        assert_eq!(live, held.into_iter().collect::<Vec<_>>(), "fails={fails}");
    }
}

#[tokio::test]
async fn a_cancelled_login_still_revokes_the_session_it_displaced() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let old = refresh_token(&client).await;
    server.set_stall_login(true);
    let _ = tokio::time::timeout(Duration::from_millis(200), client.login("alice", "pw")).await;
    eventually("the displaced token was revoked", || {
        server.logouts().contains(&old)
    })
    .await;
}

/// A login queued behind the refresh lock when sign-out runs installs nothing afterwards.
#[tokio::test]
async fn a_login_queued_before_logout_installs_nothing() {
    let server = strict().await;
    let client = BrookClient::new(CoreConfig::new(&server.base).unwrap()).unwrap();
    let client = Arc::new(client);
    let held = client.session.refresh_lock.clone().lock_owned().await;
    let c = client.clone();
    let login = tokio::spawn(async move { c.login("alice", "pw").await });
    tokio::time::sleep(Duration::from_millis(100)).await; // queued on the lock
    client.logout().await;
    drop(held);
    let result = tokio::time::timeout(WAIT, login).await.unwrap().unwrap();
    assert!(result.is_err(), "a superseded login reported success");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        client.session.snapshot().await.1.is_none(),
        "signed back in"
    );
    assert!(logged_out(&client));
    assert!(server.live_refresh_tokens("alice").is_empty());
    // Stale before it ran: it never even sent the password (nothing issued, nothing to revoke).
    assert!(
        server.logouts().is_empty(),
        "the stale login still signed in on the server"
    );
}

/// A login already sent when sign-out runs: its pair lands afterwards, is not installed, and
/// is revoked.
#[tokio::test]
async fn a_login_landing_after_logout_installs_nothing() {
    let server = strict().await;
    let client = Arc::new(BrookClient::new(CoreConfig::new(&server.base).unwrap()).unwrap());
    let gate = server.gate_login();
    let c = client.clone();
    let login = tokio::spawn(async move { c.login("alice", "pw").await });
    eventually("the server issued", || {
        !server.live_refresh_tokens("alice").is_empty()
    })
    .await;
    client.logout().await;
    gate.add_permits(1);
    let result = tokio::time::timeout(WAIT, login).await.unwrap().unwrap();
    assert!(result.is_err(), "a superseded login reported success");
    assert!(
        client.session.snapshot().await.1.is_none(),
        "signed back in"
    );
    assert!(logged_out(&client));
    eventually("the late pair was revoked", || {
        server.live_refresh_tokens("alice").is_empty()
    })
    .await;
}

/// A login whose caller is cancelled after the server issued the pair still installs it (its
/// task owns the lock and the install); nothing is orphaned.
#[tokio::test]
async fn a_login_cancelled_after_issue_installs_its_pair() {
    let server = strict().await;
    let client = Arc::new(BrookClient::new(CoreConfig::new(&server.base).unwrap()).unwrap());
    let gate = server.gate_login();
    let c = client.clone();
    let login = tokio::spawn(async move { c.login("alice", "pw").await });
    eventually("the server issued", || {
        !server.live_refresh_tokens("alice").is_empty()
    })
    .await;
    login.abort();
    gate.add_permits(1);
    let issued = server.live_refresh_tokens("alice");
    eventually("the pair was installed", || {
        matches!(*client.state().borrow(), AuthState::LoggedIn(_))
    })
    .await;
    assert_eq!(
        Some(issued[0].clone()),
        client.session.snapshot().await.1.map(|s| s.refresh_token)
    );
}

// ---- refresh ----

/// A refresh whose caller is cancelled after the server rotated keeps its lock and commits;
/// a second refresh waits for it and sends nothing (the old token would be rejected, and a
/// rejection signs the user out).
#[tokio::test]
async fn a_cancelled_refresh_caller_keeps_single_flight() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let gate = server.gate_refresh();
    let seen = client.session.snapshot().await.0;
    let r = refresher(&client);
    let first = tokio::spawn(async move { r.refresh(seen).await });
    eventually("the server rotated", || server.refresh_calls() == 1).await;
    first.abort();
    let r = refresher(&client);
    let second = tokio::spawn(async move { r.refresh(seen).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    gate.add_permits(1);
    let outcome = tokio::time::timeout(WAIT, second).await.unwrap().unwrap();
    assert!(
        matches!(outcome, Ok(RefreshOutcome::Committed)),
        "{outcome:?}"
    );
    assert_eq!(server.refresh_calls(), 1, "the old token was sent again");
    assert!(matches!(*client.state().borrow(), AuthState::LoggedIn(_)));
    assert_eq!(
        server.live_refresh_tokens("alice"),
        vec![refresh_token(&client).await]
    );
}

#[tokio::test]
async fn a_rejected_refresh_publishes_logged_out() {
    let server = TestServer::start().await;
    let client = signed_in(&server, "alice").await;
    server.set_refresh_mode(RefreshMode::Fail(401));
    let seen = client.session.snapshot().await.0;
    let outcome = refresher(&client).refresh(seen).await;
    assert!(
        matches!(outcome, Ok(RefreshOutcome::Rejected)),
        "{outcome:?}"
    );
    assert!(logged_out(&client));
}

// ---- drop ----

/// With no persisted session a dropped client is a signed-out one: its token is revoked, and
/// the drop may happen on a thread outside any runtime (a Swift thread releasing the client).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_client_revokes_its_session_even_off_the_runtime() {
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let token = refresh_token(&client).await;
    std::thread::spawn(move || drop(client))
        .join()
        .expect("dropping the client off the runtime panicked");
    eventually("the dropped session was revoked", || {
        server.logouts().contains(&token)
    })
    .await;
    assert!(server.live_refresh_tokens("alice").is_empty());
}

/// Work still in flight when the client is dropped cannot commit into the abandoned store:
/// each lands after the drop and its pair is revoked.
#[tokio::test]
async fn in_flight_work_landing_after_drop_leaves_no_live_token() {
    // A refresh.
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let gate = server.gate_refresh();
    let seen = client.session.snapshot().await.0;
    let r = refresher(&client);
    let refresh = tokio::spawn(async move { r.refresh(seen).await });
    eventually("the server rotated", || server.refresh_calls() == 1).await;
    drop(client);
    gate.add_permits(1);
    let _ = tokio::time::timeout(WAIT, refresh).await.unwrap();
    eventually("no live refresh token (refresh)", || {
        server.live_refresh_tokens("alice").is_empty()
    })
    .await;

    // A password change.
    let server = strict().await;
    let client = signed_in(&server, "alice").await;
    let gate = server.gate_password();
    let c = client.clone();
    let change =
        tokio::spawn(async move { c.change_password("old-pass-1", "new-pass-2", true).await });
    eventually("the server committed the change", || {
        server
            .requests()
            .iter()
            .any(|(p, _, _)| p == "/auth/password")
    })
    .await;
    drop(client); // the task's clone is the last owner; it goes when the task ends
    gate.add_permits(1);
    let _ = tokio::time::timeout(WAIT, change).await.unwrap();
    eventually("no live refresh token (password change)", || {
        server.live_refresh_tokens("alice").is_empty()
    })
    .await;

    // A login.
    let server = strict().await;
    let client = Arc::new(BrookClient::new(CoreConfig::new(&server.base).unwrap()).unwrap());
    let gate = server.gate_login();
    let c = client.clone();
    let login = tokio::spawn(async move { c.login("alice", "pw").await });
    eventually("the server issued", || {
        !server.live_refresh_tokens("alice").is_empty()
    })
    .await;
    login.abort();
    drop(client);
    gate.add_permits(1);
    eventually("no live refresh token (login)", || {
        server.live_refresh_tokens("alice").is_empty()
    })
    .await;
    let _ = PasswordMode::Ok; // (keeps the import meaningful across the three cases)
}

/// The background loops end with their client, from whatever wait they are in.
#[tokio::test]
async fn the_loops_end_when_the_client_is_dropped() {
    // Connected and idle (the refresh loop asleep, the socket waiting for frames).
    let mut server = strict().await;
    let client = signed_in(&server, "alice").await;
    client.start_realtime().await.unwrap();
    let mut peer = server.accept().await;
    peer.accept_auth().await;
    let tasks = client.take_tasks();
    assert_eq!(tasks.len(), 2);
    drop(client);
    for t in tasks {
        tokio::time::timeout(WAIT, t)
            .await
            .expect("a loop outlived its client")
            .unwrap();
    }

    // In a reconnect backoff.
    let mut server = strict().await;
    let client = signed_in(&server, "alice").await;
    client.start_realtime().await.unwrap();
    let mut peer = server.accept().await;
    peer.accept_auth().await;
    peer.close(1000, "bye").await; // the socket now backs off before reconnecting
    tokio::time::sleep(Duration::from_millis(50)).await;
    let tasks = client.take_tasks();
    drop(client);
    for t in tasks {
        tokio::time::timeout(WAIT, t)
            .await
            .expect("a loop outlived its client in backoff")
            .unwrap();
    }
}

/// A loop waiting for a minting task (the socket's refresh after an auth close) ends at once
/// on drop; the minting task itself finishes, and its pair is revoked.
#[tokio::test]
async fn a_loop_waiting_for_a_refresh_ends_and_the_refresh_is_revoked() {
    let mut server = strict().await;
    let client = signed_in(&server, "alice").await;
    client.start_realtime().await.unwrap();
    let mut peer = server.accept().await;
    peer.accept_auth().await;
    let gate = server.gate_refresh();
    peer.close(1008, "token_expired").await; // the socket refreshes before reconnecting
    eventually("the server rotated", || server.refresh_calls() == 1).await;
    let tasks = client.take_tasks();
    drop(client);
    for t in tasks {
        tokio::time::timeout(WAIT, t)
            .await
            .expect("a loop kept waiting for the refresh after its client was dropped")
            .unwrap();
    }
    gate.add_permits(1); // only now does the refresh's response land
    eventually("the late pair was revoked", || {
        server.live_refresh_tokens("alice").is_empty()
    })
    .await;
}

/// Publishing is part of the same store write as the change it reports: whatever order logins
/// and sign-outs race in, the published state always agrees with the store at rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_published_state_always_matches_the_store() {
    let server = TestServer::start().await;
    let client = Arc::new(BrookClient::new(CoreConfig::new(&server.base).unwrap()).unwrap());
    for round in 0..150 {
        let c = client.clone();
        let login = tokio::spawn(async move { c.login("alice", "pw").await });
        if round % 3 != 0 {
            tokio::task::yield_now().await;
        }
        let c = client.clone();
        let logout = tokio::spawn(async move { c.logout().await });
        let _ = login.await.unwrap();
        logout.await.unwrap();
        let held = client.session.snapshot().await.1.is_some();
        let published = matches!(*client.state().borrow(), AuthState::LoggedIn(_));
        assert_eq!(
            held, published,
            "round {round}: store and published state disagree"
        );
        client.logout().await;
    }
}
