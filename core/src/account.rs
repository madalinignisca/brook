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
use tokio::sync::OwnedMutexGuard;
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
        // Against the token the successful attempt used: after a 401 retry that is the rotated
        // one, not the one held at the start.
        match self
            .session
            .commit_refresh(&used_refresh, pair.access_token, pair.refresh_token)
            .await
        {
            RefreshApplied::Committed => Ok(out.other_devices_signed_out),
            RefreshApplied::Discarded => Err(Error::NotAuthenticated),
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
        let lock: OwnedMutexGuard<()> = self.session.refresh_lock.clone().lock_owned().await;
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
async fn account_error(resp: Response) -> Error {
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
