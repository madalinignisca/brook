//! Account operations: password change and admin password reset (docs/PROTOCOL.md, auth).
//!
//! The server revokes **every** refresh token of the user on a password change, the caller's
//! included, and answers with a fresh pair. A background refresh that sent the old token after
//! that would be rejected, and core's rejection path signs the user out. So the change holds the
//! single-flight refresh lock from before its request until the fresh pair is committed; login
//! takes the same lock, so no pair can be installed that the change is about to revoke.

use reqwest::{RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::client::{refresh_once, RefreshOutcome, Refresher, TokenPair};
use crate::session_store::{RefreshApplied, Revision, SessionStore};
use crate::{BrookClient, Error, Result};

/// A user as the admin user list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UserSummary {
    /// User id.
    pub id: String,
    /// Login handle.
    pub handle: String,
    /// Display name.
    pub display_name: String,
    /// `member` or `admin`.
    pub global_role: String,
}

/// `POST /auth/password`'s answer: the new pair, and what the server did about the other
/// devices. Absent on a server from before `sign_out_other_devices` (it revoked their refresh
/// tokens; their access tokens lived out their 15 minutes).
#[derive(Deserialize)]
struct PasswordChangeOut {
    #[serde(flatten)]
    pair: TokenPair,
    other_devices_signed_out: Option<bool>,
}

/// The signed-in user with their second-factor state (`GET /auth/me`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Me {
    /// Who is signed in.
    pub user: crate::User,
    /// Whether TOTP two-factor sign-in is on (the app shows Turn On or Turn Off).
    pub totp_enabled: bool,
    /// Unused recovery codes, when TOTP is on (the app warns when it runs low).
    pub recovery_codes_left: Option<u32>,
}

/// A started TOTP enrolment: the `otpauth://` URI to show as a QR code (rendered on the device)
/// and as a key for manual entry. It carries the secret, so it is never shown in `Debug`.
#[derive(Clone, Deserialize)]
pub struct TotpEnrollment {
    otpauth_uri: String,
    expires_in: u64,
}

impl TotpEnrollment {
    /// The URI to render as a QR code; the secret inside it is the manual-entry key.
    pub fn otpauth_uri(&self) -> &str {
        &self.otpauth_uri
    }

    /// Seconds until the server drops this enrolment (then enrol again, with a new QR code).
    pub fn expires_in(&self) -> u64 {
        self.expires_in
    }
}

impl std::fmt::Debug for TotpEnrollment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TotpEnrollment")
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive() // the URI carries the secret
    }
}

/// The second factor that confirms turning TOTP off or replacing the recovery codes.
#[derive(Clone)]
pub enum SecondFactor {
    /// A 6-digit code from the authenticator.
    Code(String),
    /// An unused recovery code.
    Recovery(String),
}

impl std::fmt::Debug for SecondFactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Code(_) => "SecondFactor::Code(..)",
            Self::Recovery(_) => "SecondFactor::Recovery(..)",
        })
    }
}

impl SecondFactor {
    fn into_json(self, mut body: serde_json::Value) -> serde_json::Value {
        match self {
            Self::Code(code) => {
                let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
                body["code"] = json!(code);
            }
            Self::Recovery(code) => body["recovery_code"] = json!(code.trim()),
        }
        body
    }
}

/// How a 401 on an account call refreshes before its single retry.
enum OnExpired {
    /// The caller holds the refresh lock: refresh directly (the single-flight path would wait
    /// for the lock this caller holds, forever).
    LockHeld,
    /// The caller does not hold it: the single-flight path, so a concurrent background refresh
    /// never sends the same token twice (the loser's rejection would sign the user out).
    SingleFlight,
}

/// What is needed to make an account call; cloned into the password change's own task.
#[derive(Clone)]
struct Ctx {
    http: reqwest::Client,
    base: Url,
    session: SessionStore,
}

impl Ctx {
    /// Send an authenticated request built by `build`, within `epoch` only. On 401 (never on a
    /// 403, which is a refused request, not an expired token): one refresh that must commit in
    /// the same session, then one retry. Returns the response and the refresh token belonging
    /// to the credentials the response was obtained with.
    async fn send(
        &self,
        epoch: u64,
        on_expired: OnExpired,
        build: impl Fn(&str) -> RequestBuilder,
    ) -> Result<(Response, String)> {
        let (sent, access, refresh) = self.credentials(epoch).await?;
        let resp = build(&access).send().await?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok((resp, refresh));
        }
        let outcome = match on_expired {
            OnExpired::LockHeld => refresh_once(&self.http, &self.base, &self.session).await?,
            // Against the revision the request was sent with, not the one when the 401 arrived:
            // a refresh that committed meanwhile already renewed it (so just retry), and a login
            // meanwhile is another session (so refresh none of it).
            OnExpired::SingleFlight => {
                let refresher = Refresher {
                    http: self.http.clone(),
                    base: self.base.clone(),
                    session: self.session.clone(),
                };
                refresher.refresh(sent).await?
            }
        };
        match outcome {
            RefreshOutcome::Committed => {}
            // Asked to wait: still signed in, so say so (not "signed out").
            RefreshOutcome::RateLimited(_) => {
                return Err(Error::Api {
                    code: "auth.rate_limited".into(),
                    message: "too many attempts; try again later".into(),
                })
            }
            _ => return Err(Error::NotAuthenticated),
        }
        let (_, access, refresh) = self.credentials(epoch).await?;
        let resp = build(&access).send().await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            return Err(Error::NotAuthenticated); // once; never a loop
        }
        Ok((resp, refresh))
    }

    /// The current revision, access and refresh token, if the session is still `epoch`.
    async fn credentials(&self, epoch: u64) -> Result<(Revision, String, String)> {
        let (rev, session) = self.session.snapshot().await;
        match session {
            Some(s) if rev.epoch == epoch => Ok((rev, s.access_token, s.refresh_token)),
            _ => Err(Error::NotAuthenticated),
        }
    }

    /// The locked section of a password change, run in its own task.
    async fn change_password(
        &self,
        epoch: u64,
        current: String,
        new: String,
        sign_out_other_devices: bool,
    ) -> Result<Option<bool>> {
        let url = self.base.join("api/v1/auth/password")?;
        // Always explicit: the server's default must not decide what the checkbox said.
        let body = json!({
            "current_password": current,
            "new_password": new,
            "sign_out_other_devices": sign_out_other_devices,
        });
        let (resp, used_refresh) = self
            .send(epoch, OnExpired::LockHeld, |access| {
                self.http.post(url.clone()).bearer_auth(access).json(&body)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(account_error(resp).await);
        }
        let out: PasswordChangeOut = resp.json().await.map_err(|_| Error::UnexpectedResponse)?;
        let pair = out.pair;
        let fresh = pair.refresh_token.clone();
        // Against the token the successful attempt used: after a 401 retry that is the rotated
        // one, not the one held at the start.
        match self
            .session
            .commit_refresh(&used_refresh, pair.access_token, pair.refresh_token, false)
            .await
        {
            RefreshApplied::Committed => Ok(out.other_devices_signed_out),
            // The client quit meanwhile; the stored copy kept the new pair for the next launch.
            RefreshApplied::Stored => Err(Error::NotAuthenticated),
            RefreshApplied::Discarded => {
                self.session.revoke_detached(fresh); // issued for a session no longer held
                Err(Error::NotAuthenticated)
            }
        }
    }
}

impl Ctx {
    /// The locked section of TOTP activation, run in its own task. Activation revokes every
    /// token issued before it, this device's included, and answers with a new pair and the
    /// recovery codes; the pair is committed like a password change's.
    async fn totp_activate(&self, epoch: u64, code: String) -> Result<Vec<String>> {
        let url = self.base.join("api/v1/auth/totp/activate")?;
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let body = json!({ "code": code });
        let (resp, used_refresh) = self
            .send(epoch, OnExpired::LockHeld, |access| {
                self.http.post(url.clone()).bearer_auth(access).json(&body)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(account_error(resp).await);
        }
        #[derive(Deserialize)]
        struct ActivateOut {
            #[serde(flatten)]
            pair: TokenPair,
            recovery_codes: Vec<String>,
        }
        let out: ActivateOut = resp.json().await.map_err(|_| Error::UnexpectedResponse)?;
        let fresh = out.pair.refresh_token.clone();
        match self
            .session
            .commit_refresh(
                &used_refresh,
                out.pair.access_token,
                out.pair.refresh_token,
                false,
            )
            .await
        {
            RefreshApplied::Committed => Ok(out.recovery_codes),
            // The client quit meanwhile; the stored copy kept the new pair for the next launch.
            RefreshApplied::Stored => Err(Error::NotAuthenticated),
            RefreshApplied::Discarded => {
                self.session.revoke_detached(fresh); // issued for a session no longer held
                Err(Error::NotAuthenticated)
            }
        }
    }
}

impl BrookClient {
    fn ctx(&self) -> Ctx {
        Ctx {
            http: self.http.clone(),
            base: self.base.clone(),
            session: self.session.clone(),
        }
    }

    /// Change the signed-in user's password. On success this device keeps a fresh token pair.
    /// With `sign_out_other_devices`, every other session loses its refresh token, access token
    /// and open socket at once (this device's socket is closed too and reconnects with the new
    /// pair); without it, the other sessions stay signed in.
    /// Returns whether the server signed the other devices out (`None`: an older server that
    /// does not say; word the confirmation from this, not from what was asked).
    /// A wrong current password is `Api { code: "auth.invalid_credentials" }`.
    pub async fn change_password(
        &self,
        current: &str,
        new: &str,
        sign_out_other_devices: bool,
    ) -> Result<Option<bool>> {
        let epoch = self.session.snapshot().await.0.epoch;
        let lock = self.session.flight().await;
        let ctx = self.ctx();
        let (current, new) = (current.to_string(), new.to_string());
        let bound = self.locked_bound;
        // Its own task: a caller that goes away (a closed sheet, a cancelled Swift task) cannot
        // stop it between the server's commit and core's. The bound is inside, so expiry ends
        // the task and releases the lock; a response arriving later commits nothing.
        let task = tokio::spawn(async move {
            let _lock = lock;
            tokio::time::timeout(
                bound,
                ctx.change_password(epoch, current, new, sign_out_other_devices),
            )
            .await
            .unwrap_or(Err(Error::Timeout))
        });
        task.await.map_err(|_| Error::UnexpectedResponse)?
    }

    /// Admin: set another (non-admin) user's password and revoke their refresh tokens. The
    /// admin re-enters their own password (a stolen access token alone cannot take over
    /// accounts); a wrong one is `Api { code: "auth.invalid_credentials" }`. Refused locally for
    /// the caller's own id (the self route asks for the current password); an admin target is
    /// `Api { code: "authz.forbidden" }` from the server.
    pub async fn admin_reset_password(
        &self,
        user_id: &str,
        admin_password: &str,
        new: &str,
    ) -> Result<()> {
        let (rev, session) = self.session.snapshot().await;
        let me = session.ok_or(Error::NotAuthenticated)?.user.id;
        if me == user_id {
            return Err(Error::Api {
                code: "invalid".into(),
                message: "use Change Password for your own account".into(),
            });
        }
        let ctx = self.ctx();
        // As a path segment (percent-encoded), never spliced into the path: an id containing
        // `/` or `..` must not address another route.
        let mut url = self.base.join("api/v1/users/")?;
        url.path_segments_mut()
            .map_err(|_| Error::MissingHost)?
            .pop_if_empty()
            .push(user_id)
            .push("password");
        let body = json!({ "admin_password": admin_password, "new_password": new });
        let (resp, _) = ctx
            .send(rev.epoch, OnExpired::SingleFlight, |access| {
                self.http.post(url.clone()).bearer_auth(access).json(&body)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(account_error(resp).await);
        }
        Ok(())
    }

    /// The signed-in user and their second-factor state.
    pub async fn me(&self) -> Result<Me> {
        #[derive(Deserialize)]
        struct MeOut {
            #[serde(flatten)]
            user: crate::User,
            #[serde(default)]
            totp_enabled: bool,
            recovery_codes_left: Option<u32>,
        }
        let epoch = self.session.snapshot().await.0.epoch;
        let url = self.base.join("api/v1/auth/me")?;
        let (resp, _) = self
            .ctx()
            .send(epoch, OnExpired::SingleFlight, |access| {
                self.http.get(url.clone()).bearer_auth(access)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(account_error(resp).await);
        }
        let out: MeOut = resp.json().await.map_err(|_| Error::UnexpectedResponse)?;
        Ok(Me {
            user: out.user,
            totp_enabled: out.totp_enabled,
            recovery_codes_left: out.recovery_codes_left,
        })
    }

    /// Change your display name and/or status line (`None`: unchanged; `Some("")` clears the
    /// status). Refused with `profile.invalid` (422): a name of 1 to 64 characters and a
    /// status of up to 100, with no control or text-direction characters. The handle never
    /// changes. Answers the updated profile; others see it through `/sync`.
    pub async fn update_profile(
        &self,
        display_name: Option<&str>,
        status_text: Option<&str>,
    ) -> Result<Me> {
        #[derive(Deserialize)]
        struct MeOut {
            #[serde(flatten)]
            user: crate::User,
            #[serde(default)]
            totp_enabled: bool,
            recovery_codes_left: Option<u32>,
        }
        let mut body = serde_json::Map::new();
        if let Some(name) = display_name {
            body.insert("display_name".into(), json!(name));
        }
        if let Some(status) = status_text {
            body.insert("status_text".into(), json!(status));
        }
        let body = serde_json::Value::Object(body);
        let epoch = self.session.snapshot().await.0.epoch;
        let url = self.base.join("api/v1/auth/me")?;
        let (resp, _) = self
            .ctx()
            .send(epoch, OnExpired::SingleFlight, |access| {
                self.http.patch(url.clone()).bearer_auth(access).json(&body)
            })
            .await?;
        // Not `account_error`: its 422 means a refused password. Here the server's own code
        // (`profile.invalid`) is the answer. A 401 never gets here (`send` answers it).
        if !resp.status().is_success() {
            return Err(crate::client::api_error(resp).await);
        }
        let out: MeOut = resp.json().await.map_err(|_| Error::UnexpectedResponse)?;
        Ok(Me {
            user: out.user,
            totp_enabled: out.totp_enabled,
            recovery_codes_left: out.recovery_codes_left,
        })
    }

    /// Start turning TOTP on: the password is re-checked (wrong: `auth.invalid_credentials`),
    /// and the enrolment's URI is returned once (409 `conflict` if TOTP is already on).
    pub async fn totp_enroll(&self, password: &str) -> Result<TotpEnrollment> {
        let body = json!({ "password": password });
        let resp = self.account_post("api/v1/auth/totp/enroll", body).await?;
        resp.json().await.map_err(|_| Error::UnexpectedResponse)
    }

    /// Finish turning TOTP on with a code from the authenticator: returns the recovery codes
    /// (show them once). Every other session is signed out; this device keeps a new pair,
    /// committed under the refresh lock like a password change. `auth.invalid_code`: try the
    /// next code; `auth.totp_enrollment_expired`: enrol again (a new QR code).
    pub async fn totp_activate(&self, code: &str) -> Result<Vec<String>> {
        let epoch = self.session.snapshot().await.0.epoch;
        let lock = self.session.flight().await;
        let ctx = self.ctx();
        let code = code.to_string();
        let bound = self.locked_bound;
        // Its own task with the bound inside, as `change_password`.
        let task = tokio::spawn(async move {
            let _lock = lock;
            tokio::time::timeout(bound, ctx.totp_activate(epoch, code))
                .await
                .unwrap_or(Err(Error::Timeout))
        });
        task.await.map_err(|_| Error::UnexpectedResponse)?
    }

    /// Turn TOTP off: the password and a second factor (a current code or a recovery code).
    pub async fn totp_disable(&self, password: &str, factor: SecondFactor) -> Result<()> {
        let body = factor.into_json(json!({ "password": password }));
        self.account_post("api/v1/auth/totp/disable", body).await?;
        Ok(())
    }

    /// Replace every recovery code (used or not) with ten new ones; show them once.
    pub async fn totp_regenerate_recovery_codes(
        &self,
        password: &str,
        factor: SecondFactor,
    ) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct CodesOut {
            recovery_codes: Vec<String>,
        }
        let body = factor.into_json(json!({ "password": password }));
        let resp = self
            .account_post("api/v1/auth/totp/recovery-codes", body)
            .await?;
        let out: CodesOut = resp.json().await.map_err(|_| Error::UnexpectedResponse)?;
        Ok(out.recovery_codes)
    }

    /// Admin: turn off another (non-admin) user's TOTP (their authenticator is lost). The admin
    /// re-enters their own password; the target is signed out everywhere. Refused locally for
    /// the caller's own id, read from the same snapshot whose epoch the request is bound to.
    pub async fn admin_reset_totp(&self, user_id: &str, admin_password: &str) -> Result<()> {
        let (rev, session) = self.session.snapshot().await;
        let me = session.ok_or(Error::NotAuthenticated)?.user.id;
        if me == user_id {
            return Err(Error::Api {
                code: "invalid".into(),
                message: "turn your own two-factor sign-in off from your account".into(),
            });
        }
        let mut url = self.base.join("api/v1/users/")?;
        url.path_segments_mut()
            .map_err(|_| Error::MissingHost)?
            .pop_if_empty()
            .push(user_id)
            .push("totp")
            .push("reset");
        let body = json!({ "admin_password": admin_password });
        let (resp, _) = self
            .ctx()
            .send(rev.epoch, OnExpired::SingleFlight, |access| {
                self.http.post(url.clone()).bearer_auth(access).json(&body)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(account_error(resp).await);
        }
        Ok(())
    }

    /// POST an account call with the usual 401 handling and body-free errors.
    async fn account_post(&self, path: &str, body: serde_json::Value) -> Result<Response> {
        let epoch = self.session.snapshot().await.0.epoch;
        let url = self.base.join(path)?;
        let (resp, _) = self
            .ctx()
            .send(epoch, OnExpired::SingleFlight, |access| {
                self.http.post(url.clone()).bearer_auth(access).json(&body)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(account_error(resp).await);
        }
        Ok(resp)
    }

    /// Admin: every user, by handle.
    pub async fn list_users(&self) -> Result<Vec<UserSummary>> {
        let epoch = self.session.snapshot().await.0.epoch;
        let url = self.base.join("api/v1/users")?;
        let (resp, _) = self
            .ctx()
            .send(epoch, OnExpired::SingleFlight, |access| {
                self.http.get(url.clone()).bearer_auth(access)
            })
            .await?;
        if !resp.status().is_success() {
            return Err(account_error(resp).await);
        }
        resp.json().await.map_err(|_| Error::UnexpectedResponse)
    }
}

/// Errors of the account endpoints, from the status and the envelope's `code` only. The body is
/// never carried: FastAPI's 422 echoes the submitted value (the password) back.
pub(crate) async fn account_error(resp: Response) -> Error {
    #[derive(Deserialize)]
    struct Envelope {
        error: Option<Code>,
    }
    #[derive(Deserialize)]
    struct Code {
        code: String,
    }
    let status = resp.status();
    let code = resp
        .json::<Envelope>()
        .await
        .ok()
        .and_then(|e| e.error)
        .map(|c| c.code);
    let (code, message) = match (status.as_u16(), code) {
        (401, _) => return Error::NotAuthenticated,
        (422, _) => ("validation".to_string(), "the new password was refused"),
        (429, _) => (
            "auth.rate_limited".to_string(),
            "too many attempts; try again later",
        ),
        (_, Some(code)) => (code, "the server refused the request"),
        (s, None) => (format!("http_{s}"), "the request failed"),
    };
    Error::Api {
        code,
        message: message.to_string(),
    }
}
