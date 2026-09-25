//! TOTP two-factor sign-in (spec docs/superpowers/specs/2026-09-25-totp-clients-design.md §4,
//! plan P2.1–P2.2).

use std::sync::Arc;
use std::time::Duration;

use crate::test_support::{RefreshMode, TestServer};
use crate::{AuthState, BrookClient, CoreConfig, Error, LoginOutcome, SecondFactor, TotpChallenge};

const WAIT: Duration = Duration::from_secs(5);

async fn totp_server() -> TestServer {
    let server = TestServer::start().await;
    server.set_refresh_mode(RefreshMode::Strict);
    server.enable_totp("alice", &["rc-1", "rc-2", "rc-3"]);
    server
}

fn client(server: &TestServer) -> Arc<BrookClient> {
    Arc::new(BrookClient::new(CoreConfig::new(&server.base).unwrap()).unwrap())
}

async fn challenge(client: &BrookClient) -> TotpChallenge {
    match client.login("alice", "pw").await.unwrap() {
        LoginOutcome::TotpRequired(c) => c,
        LoginOutcome::LoggedIn(_) => panic!("signed in with the password alone"),
    }
}

fn state(client: &BrookClient) -> AuthState {
    client.state().borrow().clone()
}

fn is_code(err: &Error, code: &str) -> bool {
    matches!(err, Error::Api { code: c, .. } if c == code)
}

#[tokio::test]
async fn login_says_it_supports_totp_and_installs_nothing_after_the_password() {
    let server = totp_server().await;
    let client = client(&server);
    let _challenge = challenge(&client).await;
    let sent = server
        .requests()
        .into_iter()
        .find(|(p, _, _)| p == "/auth/login")
        .unwrap()
        .2;
    assert_eq!(sent["supports_totp"], true);
    assert!(
        client.session.snapshot().await.1.is_none(),
        "installed after the password"
    );
    assert_eq!(state(&client), AuthState::Authenticating);
    assert!(matches!(
        client.list_channels().await,
        Err(Error::NotAuthenticated)
    ));
}

#[tokio::test]
async fn a_user_without_totp_signs_in_as_before() {
    let server = TestServer::start().await;
    let client = client(&server);
    assert!(matches!(
        client.login("bob", "pw").await.unwrap(),
        LoginOutcome::LoggedIn(_)
    ));
}

#[tokio::test]
async fn the_right_code_signs_in_and_consumes_the_challenge() {
    let server = totp_server().await;
    let client = client(&server);
    let c = challenge(&client).await;
    let left = client.complete_totp(&c, "123456").await.unwrap();
    assert_eq!(left, None);
    assert!(matches!(state(&client), AuthState::LoggedIn(_)));
    assert!(client.session.snapshot().await.1.is_some());
    // Consumed: a second completion changes nothing (never "back to the password").
    let again = client.complete_totp(&c, "123456").await.unwrap_err();
    assert!(matches!(again, Error::ChallengeSuperseded), "{again:?}");
    assert!(matches!(state(&client), AuthState::LoggedIn(_)));
}

/// A wrong code is `auth.invalid_code` and keeps the challenge; it never triggers a refresh
/// (it's a 403, and there is no session yet anyway).
#[tokio::test]
async fn a_wrong_code_keeps_the_challenge() {
    let server = totp_server().await;
    let client = client(&server);
    let c = challenge(&client).await;
    let err = client.complete_totp(&c, "000000").await.unwrap_err();
    assert!(is_code(&err, "auth.invalid_code"), "{err:?}");
    assert_eq!(server.refresh_calls(), 0);
    assert_eq!(state(&client), AuthState::Authenticating);
    client.complete_totp(&c, "123456").await.unwrap();
    assert!(matches!(state(&client), AuthState::LoggedIn(_)));
}

#[tokio::test]
async fn an_expired_challenge_ends_it() {
    let server = totp_server().await;
    let client = client(&server);
    let c = challenge(&client).await;
    server.expire_pending();
    let err = client.complete_totp(&c, "123456").await.unwrap_err();
    assert!(is_code(&err, "auth.totp_expired"), "{err:?}");
    assert_eq!(state(&client), AuthState::LoggedOut);
    let again = client.complete_totp(&c, "123456").await.unwrap_err();
    assert!(
        matches!(again, Error::ChallengeSuperseded),
        "kept after expiry: {again:?}"
    );
}

#[tokio::test]
async fn a_recovery_code_signs_in_and_reports_what_is_left() {
    let server = totp_server().await;
    let client = client(&server);
    let c = challenge(&client).await;
    assert_eq!(client.complete_recovery(&c, "rc-2").await.unwrap(), Some(2));
    assert!(matches!(state(&client), AuthState::LoggedIn(_)));
}

#[tokio::test]
async fn cancel_ends_only_its_own_challenge_and_is_idempotent() {
    let server = totp_server().await;
    let client = client(&server);
    let old = challenge(&client).await;
    let new = challenge(&client).await; // a second login: the first challenge is stale
    client.cancel_totp(&old).await; // a stale Back never cancels the newer challenge
    client.complete_totp(&new, "123456").await.unwrap();
    assert!(matches!(state(&client), AuthState::LoggedIn(_)));
    client.cancel_totp(&new).await; // consumed already: nothing happens
    client.cancel_totp(&new).await;
    assert!(matches!(state(&client), AuthState::LoggedIn(_)));
}

/// A completion in flight when the user goes Back (or signs out, or starts over) installs
/// nothing; the pair the server issued for it is revoked.
#[tokio::test]
async fn a_completion_landing_after_cancel_installs_nothing() {
    let server = totp_server().await;
    let client = client(&server);
    let c = challenge(&client).await;
    let gate = server.gate_totp();
    let cl = client.clone();
    let c2 = c.clone();
    let done = tokio::spawn(async move { cl.complete_totp(&c2, "123456").await });
    let deadline = tokio::time::Instant::now() + WAIT;
    while server.live_refresh_tokens("alice").is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the server never issued"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client.cancel_totp(&c).await;
    gate.add_permits(1);
    let err = tokio::time::timeout(WAIT, done)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, Error::ChallengeSuperseded), "{err:?}");
    assert!(
        client.session.snapshot().await.1.is_none(),
        "signed in after Back"
    );
    assert_eq!(state(&client), AuthState::LoggedOut);
    let deadline = tokio::time::Instant::now() + WAIT;
    while !server.live_refresh_tokens("alice").is_empty() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the late pair was not revoked"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_logout_ends_the_challenge() {
    let server = totp_server().await;
    let client = client(&server);
    let c = challenge(&client).await;
    client.logout().await;
    let err = client.complete_totp(&c, "123456").await.unwrap_err();
    assert!(matches!(err, Error::ChallengeSuperseded), "{err:?}");
    assert!(client.session.snapshot().await.1.is_none());
    assert!(
        server.requests().iter().all(|(p, _, _)| p != "/auth/totp"),
        "a superseded challenge was still sent"
    );
}

/// The pending token is never an access token: not in `Debug`, not a bearer, not on the WS.
#[tokio::test]
async fn the_pending_token_stays_inside_the_challenge() {
    let server = totp_server().await;
    let client = client(&server);
    let c = challenge(&client).await;
    let shown = format!("{c:?}");
    assert!(
        !shown.contains("totp-"),
        "the token leaked into Debug: {shown}"
    );
    assert!(c.seconds_left() > 0 && c.seconds_left() <= 300);
    assert!(
        client.start_realtime().await.is_err(),
        "a socket opened with no session"
    );
    assert!(server
        .requests()
        .iter()
        .all(|(_, bearer, _)| !bearer.starts_with("totp-")));
}

// ---- management (plan P2.3) ----

async fn signed_in_bob(server: &TestServer) -> Arc<BrookClient> {
    let client = client(server);
    assert!(matches!(
        client.login("bob", "pw").await.unwrap(),
        LoginOutcome::LoggedIn(_)
    ));
    client
}

async fn held_refresh(client: &BrookClient) -> String {
    client.session.snapshot().await.1.unwrap().refresh_token
}

#[tokio::test]
async fn me_reports_the_second_factor() {
    let server = totp_server().await;
    let bob = signed_in_bob(&server).await;
    let me = bob.me().await.unwrap();
    assert_eq!(me.user.handle, "bob");
    assert!(!me.totp_enabled);
    assert_eq!(me.recovery_codes_left, None);
}

#[tokio::test]
async fn enrol_returns_the_uri_once_and_never_shows_it_in_debug() {
    let server = totp_server().await;
    let bob = signed_in_bob(&server).await;
    let enrollment = bob.totp_enroll("pw").await.unwrap();
    assert!(enrollment.otpauth_uri().starts_with("otpauth://totp/"));
    assert_eq!(enrollment.expires_in(), 600);
    let shown = format!("{enrollment:?}");
    assert!(
        !shown.contains("JBSWY3"),
        "the secret leaked into Debug: {shown}"
    );
    let err = bob.totp_enroll("Sekret-Wrong-1").await.unwrap_err();
    assert!(is_code(&err, "auth.invalid_credentials"), "{err:?}");
    assert!(!format!("{err} {err:?}").contains("Sekret-Wrong-1"));
    assert_eq!(server.refresh_calls(), 0, "a refused password refreshed");
}

/// Activation revokes every token issued before it, this device's included, and returns a new
/// pair: committed under the refresh lock, so this device stays signed in.
#[tokio::test]
async fn activation_keeps_this_device_signed_in_with_the_new_pair() {
    let server = totp_server().await;
    let bob = signed_in_bob(&server).await;
    let before = held_refresh(&bob).await;
    bob.totp_enroll("pw").await.unwrap();
    let codes = bob.totp_activate("123456").await.unwrap();
    assert_eq!(codes.len(), 10);
    let after = held_refresh(&bob).await;
    assert_ne!(after, before);
    assert_eq!(server.live_refresh_tokens("bob"), vec![after]);
    assert!(matches!(state(&bob), AuthState::LoggedIn(_)));
    assert!(server.totp_enabled("bob"));
    let me = bob.me().await.unwrap(); // the new access token works
    assert!(me.totp_enabled);
    assert_eq!(me.recovery_codes_left, Some(10));
}

/// The race the lock exists for: a background refresh arriving while activation is in flight
/// waits, sees the new pair, and sends nothing (the old token is revoked by then, and its
/// rejection would sign the user out right after turning 2FA on).
#[tokio::test]
async fn a_refresh_racing_activation_never_signs_the_user_out() {
    let server = totp_server().await;
    let bob = signed_in_bob(&server).await;
    bob.totp_enroll("pw").await.unwrap();
    let gate = server.gate_password(); // activation's response is held after the cutoff
    let seen = bob.session.snapshot().await.0;
    let b = bob.clone();
    let activate = tokio::spawn(async move { b.totp_activate("123456").await });
    let deadline = tokio::time::Instant::now() + WAIT;
    while !server.totp_enabled("bob") {
        assert!(tokio::time::Instant::now() < deadline, "never activated");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let r = crate::client::Refresher {
        http: bob.http.clone(),
        base: bob.base.clone(),
        session: bob.session.clone(),
    };
    let refresh = tokio::spawn(async move { r.refresh(seen).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        server.refresh_calls(),
        0,
        "refreshed while activation held the lock"
    );
    gate.add_permits(1);
    tokio::time::timeout(WAIT, activate)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let _ = tokio::time::timeout(WAIT, refresh).await.unwrap().unwrap();
    assert_eq!(server.refresh_calls(), 0, "the revoked token was sent");
    assert!(
        matches!(state(&bob), AuthState::LoggedIn(_)),
        "signed out by turning on 2FA"
    );
}

#[tokio::test]
async fn an_expired_enrolment_says_so() {
    let server = totp_server().await;
    let bob = signed_in_bob(&server).await;
    bob.totp_enroll("pw").await.unwrap();
    server.expire_enrollment();
    let err = bob.totp_activate("123456").await.unwrap_err();
    assert!(is_code(&err, "auth.totp_enrollment_expired"), "{err:?}");
    assert!(matches!(state(&bob), AuthState::LoggedIn(_)));
}

#[tokio::test]
async fn disable_and_new_recovery_codes_need_the_password_and_a_second_factor() {
    let server = totp_server().await;
    let bob = signed_in_bob(&server).await;
    bob.totp_enroll("pw").await.unwrap();
    bob.totp_activate("123456").await.unwrap();
    let err = bob
        .totp_regenerate_recovery_codes("pw", SecondFactor::Code("000000".into()))
        .await
        .unwrap_err();
    assert!(is_code(&err, "auth.invalid_code"), "{err:?}");
    let fresh = bob
        .totp_regenerate_recovery_codes("pw", SecondFactor::Code("123456".into()))
        .await
        .unwrap();
    assert_eq!(fresh.len(), 10);
    bob.totp_disable("pw", SecondFactor::Recovery(fresh[0].clone()))
        .await
        .unwrap();
    assert!(!server.totp_enabled("bob"));
    let sent = server
        .requests()
        .into_iter()
        .filter(|(p, _, _)| p == "/auth/totp/disable")
        .map(|(_, _, b)| b)
        .collect::<Vec<_>>();
    assert_eq!(
        sent,
        vec![serde_json::json!({ "password": "pw", "recovery_code": fresh[0] })]
    );
}

#[tokio::test]
async fn admin_reset_of_totp_refuses_oneself_locally() {
    let server = totp_server().await;
    let admin = signed_in_bob(&server).await; // the fake treats any bearer as allowed
    let me = admin.session.snapshot().await.1.unwrap().user.id;
    let err = admin.admin_reset_totp(&me, "pw").await.unwrap_err();
    assert!(is_code(&err, "invalid"), "{err:?}");
    assert!(server
        .requests()
        .iter()
        .all(|(p, _, _)| !p.ends_with("/totp/reset")));
    admin.admin_reset_totp("id-alice", "pw").await.unwrap();
    assert!(!server.totp_enabled("alice"));
}

// ---- review round 1 ----

/// A challenge belongs to the client (session store) that produced it: another client's first
/// challenge has the same generation number, and must still be refused, with nothing sent.
#[tokio::test]
async fn a_challenge_is_bound_to_its_own_client() {
    let server = totp_server().await;
    let a = client(&server);
    let b = client(&server);
    let from_a = challenge(&a).await;
    let _from_b = challenge(&b).await;
    let err = b.complete_totp(&from_a, "123456").await.unwrap_err();
    assert!(matches!(err, Error::ChallengeSuperseded), "{err:?}");
    assert!(b.session.snapshot().await.1.is_none());
    assert!(
        server.requests().iter().all(|(p, _, _)| p != "/auth/totp"),
        "another client's pending token was sent"
    );
    b.cancel_totp(&from_a).await; // and cancelling it is a no-op on b
    assert_eq!(state(&b), AuthState::Authenticating);
}

/// The server's error text never reaches ours: a body echoing the submitted code (or the
/// pending token) stays out of the error's Display and Debug.
#[tokio::test]
async fn a_refused_code_never_reaches_the_error_text() {
    let server = totp_server().await;
    let client = client(&server);
    let c = challenge(&client).await;
    let err = client.complete_totp(&c, "987654").await.unwrap_err();
    assert!(is_code(&err, "auth.invalid_code"), "{err:?}");
    let shown = format!("{err} {err:?}");
    assert!(
        !shown.contains("987654") && !shown.contains("totp-"),
        "{shown}"
    );
}

/// A completion refused by the server after the user went Back reports the Back, not the
/// server's verdict on a challenge that no longer matters.
#[tokio::test]
async fn a_refusal_after_cancel_is_superseded() {
    let server = totp_server().await;
    let client = client(&server);
    let c = challenge(&client).await;
    let gate = server.gate_totp_errors();
    let cl = client.clone();
    let c2 = c.clone();
    let done = tokio::spawn(async move { cl.complete_totp(&c2, "000000").await });
    let deadline = tokio::time::Instant::now() + WAIT;
    while server.requests().iter().all(|(p, _, _)| p != "/auth/totp") {
        assert!(tokio::time::Instant::now() < deadline, "never sent");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client.cancel_totp(&c).await;
    gate.add_permits(1);
    let err = tokio::time::timeout(WAIT, done)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, Error::ChallengeSuperseded), "{err:?}");
}

/// A code the server refuses as malformed (422: a recovery code over its length bound) is a
/// wrong code to the user, never "the new password was refused", and never echoed.
#[tokio::test]
async fn a_malformed_code_is_a_wrong_code() {
    let server = totp_server().await;
    let client = client(&server);
    let c = challenge(&client).await;
    let pasted = "x".repeat(100);
    let err = client.complete_recovery(&c, &pasted).await.unwrap_err();
    assert!(is_code(&err, "auth.invalid_code"), "{err:?}");
    assert!(!format!("{err} {err:?}").contains(&pasted));
}
