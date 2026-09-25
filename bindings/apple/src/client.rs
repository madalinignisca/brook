//! The client object exported to Swift.

use std::sync::Arc;

use brook_core::{BrookClient, CoreConfig, LoginOutcome};

use std::sync::atomic::{AtomicBool, Ordering};

use brook_core::ServerEvent;
use tokio::sync::broadcast::error::RecvError;

use crate::call::{
    run, EngineAdapter, FfiCallHandle, FfiMediaEngine, FfiServerEvent, ServerEventListener,
};
use crate::listener::{subscribe_receiver, AuthStateListener, Subscription};
use crate::runtime::runtime;
use crate::types::{
    FfiChannel, FfiMe, FfiSecondFactor, FfiTotpChallenge, FfiTotpEnrollment, FfiUserSummary,
    LoginError, LoginResult,
};

/// Swift-facing wrapper around [`BrookClient`].
#[derive(uniffi::Object)]
pub struct FfiBrookClient {
    inner: Arc<BrookClient>,
}

#[uniffi::export]
impl FfiBrookClient {
    /// `allow_insecure_http` permits plain `http` to **any** host (dev only: the password
    /// and tokens travel in cleartext). Without it, `http` is accepted only for loopback.
    #[uniffi::constructor]
    pub fn new(base_url: String, allow_insecure_http: bool) -> Result<Arc<Self>, LoginError> {
        let config = CoreConfig::with_options(&base_url, allow_insecure_http)?;
        Ok(Arc::new(Self {
            inner: Arc::new(BrookClient::new(config)?),
        }))
    }

    /// Observe authentication state (latest state wins; see [`AuthStateListener`]).
    pub fn subscribe(&self, listener: Arc<dyn AuthStateListener>) -> Arc<Subscription> {
        subscribe_receiver(self.inner.state(), listener)
    }

    /// Keep the session across launches in `slot` (the Keychain), with sign-out fences under
    /// `data_dir`. Call once, before signing in or restoring.
    pub fn enable_persistence(&self, slot: Arc<dyn crate::keyslot::FfiKeySlot>, data_dir: String) {
        self.inner.enable_persistence(
            Arc::new(crate::keyslot::SlotAdapter(slot)),
            std::path::PathBuf::from(data_dir),
        );
    }

    /// At launch: sign in with the stored session, if there's a usable one.
    pub async fn restore(&self) -> crate::types::FfiRestoreOutcome {
        let inner = Arc::clone(&self.inner);
        match runtime().spawn(async move { inner.restore().await }).await {
            Ok(outcome) => outcome.into(),
            Err(_) => crate::types::FfiRestoreOutcome::Offline,
        }
    }

    /// False only when the last sign-out couldn't make the stored session unusable.
    pub fn sign_out_complete(&self) -> bool {
        self.inner.sign_out_complete()
    }

    /// Core's authentication state right now. The subscription keeps only the latest value
    /// (a quick `LoggedIn` then `LoggedOut` can arrive as just `LoggedOut`), so the app reads
    /// the truth here when its own login completes.
    pub fn auth_state(&self) -> crate::types::FfiAuthState {
        self.inner.state().borrow().clone().into()
    }

    /// Sign out: the session ends at once, `LoggedOut` is published, and the server is asked
    /// to revoke the refresh token (best-effort). Never fails.
    pub async fn logout(&self) {
        let inner = Arc::clone(&self.inner);
        let _ = run(async move {
            inner.logout().await;
            Ok::<(), brook_core::Error>(())
        })
        .await;
    }

    /// Open the realtime socket (idempotent). Subscribe to events first so `Ready` and the
    /// `channel.call` snapshot sent right after it are not missed.
    pub async fn start_realtime(&self) -> Result<(), LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.start_realtime().await }).await
    }

    /// Channels and DMs the signed-in user belongs to.
    pub async fn list_channels(&self) -> Result<Vec<FfiChannel>, LoginError> {
        let inner = Arc::clone(&self.inner);
        let channels = run(async move { inner.list_channels().await }).await?;
        Ok(channels.into_iter().map(Into::into).collect())
    }

    /// Change the signed-in user's password; this device keeps a fresh token pair. Returns
    /// whether the server signed the other devices out (nil: an older server that does not say).
    /// A wrong current password is `Api { code: "auth.invalid_credentials" }`.
    pub async fn change_password(
        &self,
        current: String,
        new: String,
        sign_out_other_devices: bool,
    ) -> Result<Option<bool>, LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move {
            inner
                .change_password(&current, &new, sign_out_other_devices)
                .await
        })
        .await
    }

    /// Admin: set another (non-admin) user's password, re-entering the admin's own password.
    pub async fn admin_reset_password(
        &self,
        user_id: String,
        admin_password: String,
        new: String,
    ) -> Result<(), LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move {
            inner
                .admin_reset_password(&user_id, &admin_password, &new)
                .await
        })
        .await
    }

    /// Admin: every user, by handle.
    pub async fn list_users(&self) -> Result<Vec<FfiUserSummary>, LoginError> {
        let inner = Arc::clone(&self.inner);
        let users = run(async move { inner.list_users().await }).await?;
        Ok(users.into_iter().map(Into::into).collect())
    }

    /// Realtime events the Apple UI uses (`Ready`, `ChannelCall`); others are skipped.
    /// Cancel (or drop) the subscription to stop.
    pub fn subscribe_events(&self, listener: Arc<dyn ServerEventListener>) -> Arc<Subscription> {
        let mut rx = self.inner.events();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancelled);
        let task = runtime().spawn(async move {
            loop {
                let event = match rx.recv().await {
                    Ok(event) => event,
                    Err(RecvError::Lagged(_)) => continue, // UI state is re-derivable
                    Err(RecvError::Closed) => break,
                };
                let mapped = match event {
                    ServerEvent::Ready => FfiServerEvent::Ready,
                    ServerEvent::ChannelCall {
                        channel_id,
                        call_id,
                        participant_count,
                    } => FfiServerEvent::ChannelCall {
                        channel_id,
                        call_id,
                        participant_count,
                    },
                    _ => continue,
                };
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                listener.on_event(mapped);
            }
        });
        Subscription::from_task(cancelled, task)
    }

    /// Join `channel_id`'s call, driving the Swift `engine`. Requires a ready socket.
    pub async fn join_call(
        &self,
        channel_id: String,
        engine: Arc<dyn FfiMediaEngine>,
        publish: bool,
    ) -> Result<Arc<FfiCallHandle>, LoginError> {
        let inner = Arc::clone(&self.inner);
        let engine: Arc<dyn brook_core::MediaEngine> = Arc::new(EngineAdapter(engine));
        let handle =
            run(async move { inner.join_call(&channel_id, engine, publish).await }).await?;
        Ok(FfiCallHandle::new(handle))
    }

    /// Log in with a local handle + password.
    pub async fn login(&self, handle: String, password: String) -> Result<LoginResult, LoginError> {
        let inner = Arc::clone(&self.inner);
        let task = runtime().spawn(async move { inner.login(&handle, &password).await });
        match task.await {
            Ok(result) => Ok(match result? {
                LoginOutcome::LoggedIn(session) => LoginResult::LoggedIn {
                    session: session.into(),
                },
                LoginOutcome::TotpRequired(challenge) => LoginResult::TotpRequired {
                    challenge: Arc::new(FfiTotpChallenge { inner: challenge }),
                },
            }),
            // A panic inside core must not cross the FFI as a crash.
            Err(_) => Err(LoginError::UnexpectedResponse),
        }
    }

    /// Finish a TOTP sign-in with a 6-digit code. `Api{auth.invalid_code}` keeps the challenge;
    /// `Api{auth.totp_expired}` ends it; `ChallengeSuperseded`: change nothing.
    pub async fn complete_totp(
        &self,
        challenge: Arc<FfiTotpChallenge>,
        code: String,
    ) -> Result<Option<u32>, LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.complete_totp(&challenge.inner, &code).await }).await
    }

    /// Finish a TOTP sign-in with a recovery code; returns how many are left.
    pub async fn complete_recovery(
        &self,
        challenge: Arc<FfiTotpChallenge>,
        recovery_code: String,
    ) -> Result<Option<u32>, LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move {
            inner
                .complete_recovery(&challenge.inner, &recovery_code)
                .await
        })
        .await
    }

    /// Back from the code step (only ends that challenge; idempotent).
    pub async fn cancel_totp(&self, challenge: Arc<FfiTotpChallenge>) {
        let inner = Arc::clone(&self.inner);
        let _ = run(async move {
            inner.cancel_totp(&challenge.inner).await;
            Ok::<(), brook_core::Error>(())
        })
        .await;
    }

    /// The signed-in user and their second-factor state.
    pub async fn me(&self) -> Result<FfiMe, LoginError> {
        let inner = Arc::clone(&self.inner);
        Ok(run(async move { inner.me().await }).await?.into())
    }

    /// Start turning TOTP on (the password is re-checked).
    pub async fn totp_enroll(
        &self,
        password: String,
    ) -> Result<Arc<FfiTotpEnrollment>, LoginError> {
        let inner = Arc::clone(&self.inner);
        let enrollment = run(async move { inner.totp_enroll(&password).await }).await?;
        Ok(Arc::new(FfiTotpEnrollment { inner: enrollment }))
    }

    /// Finish turning TOTP on; returns the recovery codes (show them once).
    pub async fn totp_activate(&self, code: String) -> Result<Vec<String>, LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.totp_activate(&code).await }).await
    }

    /// Turn TOTP off: the password and a second factor.
    pub async fn totp_disable(
        &self,
        password: String,
        factor: FfiSecondFactor,
    ) -> Result<(), LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.totp_disable(&password, factor.into()).await }).await
    }

    /// Replace every recovery code with ten new ones (show them once).
    pub async fn totp_regenerate_recovery_codes(
        &self,
        password: String,
        factor: FfiSecondFactor,
    ) -> Result<Vec<String>, LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move {
            inner
                .totp_regenerate_recovery_codes(&password, factor.into())
                .await
        })
        .await
    }

    /// Admin: turn off another (non-admin) user's TOTP; they are signed out everywhere.
    pub async fn admin_reset_totp(
        &self,
        user_id: String,
        admin_password: String,
    ) -> Result<(), LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.admin_reset_totp(&user_id, &admin_password).await }).await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    // Distinct sentinels: a swapped mapping cannot pass by accident.
    const ACCESS: &str = "access-sentinel-A";
    const REFRESH: &str = "refresh-sentinel-R";

    async fn mock_login_ok() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": ACCESS, "refresh_token": REFRESH, "token_type": "bearer"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/auth/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "u1", "handle": "alice", "display_name": "Alice", "global_role": "admin"
            })))
            .mount(&server)
            .await;
        server
    }

    /// Test 1: Swift polls our futures on its own executor, with no Tokio runtime entered.
    /// A plain thread + `block_on` reproduces that; without the runtime hop, reqwest panics.
    #[test]
    fn login_works_when_polled_outside_any_tokio_runtime() {
        let mock_rt = tokio::runtime::Runtime::new().unwrap();
        let server = mock_rt.block_on(mock_login_ok());
        let uri = server.uri();

        let result = std::thread::spawn(move || {
            let client = FfiBrookClient::new(uri, false).unwrap();
            futures::executor::block_on(client.login("alice".into(), "pw".into()))
        })
        .join()
        .expect("login panicked when polled outside a Tokio runtime");

        assert!(matches!(result, Ok(LoginResult::LoggedIn { .. })));
        drop(server);
    }

    /// Test 2: tokens cross the mapping intact and unswapped.
    #[tokio::test]
    async fn login_success_carries_exact_tokens_and_user() {
        let server = mock_login_ok().await;
        let client = FfiBrookClient::new(server.uri(), false).unwrap();

        let LoginResult::LoggedIn { session } =
            client.login("alice".into(), "pw".into()).await.unwrap()
        else {
            panic!("expected a session");
        };

        assert_eq!(session.access_token, ACCESS);
        assert_eq!(session.refresh_token, REFRESH);
        assert_eq!(session.user.handle, "alice");
    }

    /// Test 3: the server's error code reaches Swift verbatim.
    #[tokio::test]
    async fn rejected_login_maps_to_api_error_code() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/login"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": { "code": "auth.invalid_credentials", "message": "Invalid handle or password" }
            })))
            .mount(&server)
            .await;
        let client = FfiBrookClient::new(server.uri(), false).unwrap();

        let err = client
            .login("alice".into(), "wrong".into())
            .await
            .unwrap_err();

        assert_eq!(
            err,
            LoginError::Api {
                code: "auth.invalid_credentials".into(),
                message: "Invalid handle or password".into(),
            }
        );
    }

    /// Test 8: the insecure-http opt-in is passed through, not assumed.
    #[test]
    fn insecure_http_to_a_remote_host_requires_the_explicit_flag() {
        let lan = "http://192.168.1.50:8080".to_string();
        assert!(matches!(
            FfiBrookClient::new(lan.clone(), false),
            Err(LoginError::InsecureServerUrl)
        ));
        assert!(FfiBrookClient::new(lan, true).is_ok());
    }
}
