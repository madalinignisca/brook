//! The Brook core client.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{broadcast, watch};
use url::Url;

use crate::session_store::{RefreshApplied, Revision, SessionStore};
use crate::ws::{self, Commands, ServerEvent, Transport};
use crate::{
    AuthState, Channel, CoreConfig, Error, Message, ReactionSummary, Result, Session, User,
};

/// Refresh the access token this long before its ~15 min server TTL elapses.
const REFRESH_INTERVAL: Duration = Duration::from_secs(600);

/// After a transient refresh failure (or while logged out), poll again this soon
/// — short enough to recover well before the access token expires.
pub(crate) const REFRESH_RETRY_INTERVAL: Duration = Duration::from_secs(15);

/// What a password login produced.
#[derive(Debug)]
pub enum LoginOutcome {
    /// Signed in.
    LoggedIn(Session),
    /// The account has TOTP on: complete the sign-in with a code (or a recovery code).
    TotpRequired(TotpChallenge),
}

/// The second step of a TOTP sign-in. Opaque: the server's pending token never leaves it (not
/// in `Debug`, never a bearer). Only the current attempt's challenge can complete.
#[derive(Clone)]
pub struct TotpChallenge {
    /// The client (session store) it came from, and that client's login attempt.
    store: u64,
    gen: u64,
    token: String,
    expires_at: tokio::time::Instant,
}

impl TotpChallenge {
    /// When the server stops accepting it (then sign in with the password again).
    pub fn expires_at(&self) -> tokio::time::Instant {
        self.expires_at
    }

    /// Seconds until it expires (0 once expired).
    pub fn seconds_left(&self) -> u64 {
        self.expires_at
            .saturating_duration_since(tokio::time::Instant::now())
            .as_secs()
    }
}

impl std::fmt::Debug for TotpChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TotpChallenge")
            .field("seconds_left", &self.seconds_left())
            .finish_non_exhaustive() // the token is never shown
    }
}

/// A password login's answer: a pair (not yet a session), or a TOTP step.
enum Issued {
    Pair(TokenPair),
    Totp { token: String, expires_in: u64 },
}

/// Shared client: networking + observable auth state.
///
/// Cheap to clone-by-`Arc` from the UI; safe to call from any async task.
pub struct BrookClient {
    pub(crate) base: Url,
    pub(crate) http: reqwest::Client,
    state_tx: Arc<watch::Sender<AuthState>>,
    state_rx: watch::Receiver<AuthState>,
    /// The active session (set on login), used to authorize chat calls + the WS.
    pub(crate) session: SessionStore,
    /// Realtime events fan-out to UI subscribers.
    events_tx: broadcast::Sender<ServerEvent>,
    /// Guards against starting the realtime task more than once.
    realtime_started: AtomicBool,
    /// Sends commands over the realtime socket (calls).
    pub(crate) commands: Commands,
    /// The socket side of `commands`, handed to the realtime task when it starts.
    transport: std::sync::Mutex<Option<Transport>>,
    /// Bound on a password change's locked section (tests shorten it).
    pub(crate) locked_bound: std::time::Duration,
    /// Dropped with the client: the background loops end on it, from whatever wait.
    shutdown: watch::Sender<()>,
    /// The background loops (refresh, realtime), for tests to observe that they end.
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for BrookClient {
    /// No session is persisted, so a dropped client is a signed-out one: fence the store (work
    /// still in flight commits nothing) and revoke what it held. Safe on any thread.
    fn drop(&mut self) {
        self.session.close_detached();
    }
}

impl BrookClient {
    /// Create a client for the given configuration.
    pub fn new(config: CoreConfig) -> Result<Self> {
        // Never follow redirects. `CoreConfig` enforces https (or loopback http) on the
        // configured URL only; a followed 307/308 would re-send the request body — the
        // password — to wherever `Location` points, plain http included. The API has no
        // reason to redirect, so a 3xx surfaces as an error instead.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.request_timeout)
            .build()?;
        let (state_tx, state_rx) = watch::channel(AuthState::LoggedOut);
        let state_tx = Arc::new(state_tx);
        let (commands, transport) = ws::command_channel();
        let (events_tx, _) = broadcast::channel(256);
        let session = SessionStore::new(state_tx.clone());
        session.set_revoke_target(http.clone(), config.base_url.clone());
        let (shutdown, _) = watch::channel(());
        Ok(Self {
            base: config.base_url,
            http,
            session,
            state_tx,
            state_rx,
            events_tx,
            realtime_started: AtomicBool::new(false),
            commands,
            transport: std::sync::Mutex::new(Some(transport)),
            locked_bound: std::time::Duration::from_secs(30),
            shutdown,
            tasks: std::sync::Mutex::default(),
        })
    }

    /// Sign out: the session ends at once (never waiting for a refresh or login in flight;
    /// whatever they bring back is revoked), `LoggedOut` is published, and the server is
    /// asked to revoke the refresh token, best-effort. No session: only the publish.
    pub async fn logout(&self) {
        self.session.note_runtime();
        if let Some(old) = self.session.sign_out(false).await {
            self.session.revoke_detached(old.refresh_token);
        }
    }

    /// Complete a TOTP sign-in with a 6-digit code. `Ok(None)`: signed in. A wrong code is
    /// `Api { code: "auth.invalid_code" }` and keeps the challenge; `auth.totp_expired` ends it
    /// (sign in with the password again); `ChallengeSuperseded`: it is no longer the current
    /// attempt (Back, a newer login, a sign-out, or already completed) and nothing changed.
    pub async fn complete_totp(
        &self,
        challenge: &TotpChallenge,
        code: &str,
    ) -> Result<Option<u32>> {
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        self.complete_challenge(challenge, json!({ "code": code }))
            .await
    }

    /// Complete a TOTP sign-in with a recovery code; returns how many unused codes are left.
    /// Errors as [`Self::complete_totp`].
    pub async fn complete_recovery(
        &self,
        challenge: &TotpChallenge,
        recovery_code: &str,
    ) -> Result<Option<u32>> {
        let code = recovery_code.trim().to_string();
        self.complete_challenge(challenge, json!({ "recovery_code": code }))
            .await
    }

    /// Back from the code step: end this challenge (only if it is still the current one; a
    /// stale Back never cancels a newer attempt). Idempotent.
    pub async fn cancel_totp(&self, challenge: &TotpChallenge) {
        if challenge.store == self.session.id() {
            self.session.end_challenge(challenge.gen).await;
        }
    }

    async fn complete_challenge(
        &self,
        challenge: &TotpChallenge,
        mut body: serde_json::Value,
    ) -> Result<Option<u32>> {
        self.session.note_runtime();
        if challenge.store != self.session.id() {
            return Err(Error::ChallengeSuperseded); // another client's: nothing sent
        }
        body["totp_token"] = json!(challenge.token);
        let gen = challenge.gen;
        let (session, http, base) = (self.session.clone(), self.http.clone(), self.base.clone());
        // Its own task holding the refresh lock, like login: a cancelled caller cannot stop it
        // between the server issuing a pair and core installing (or revoking) it.
        let task = tokio::spawn(async move {
            let _flight = session.refresh_lock.clone().lock_owned().await;
            if !session.challenge_current(gen).await {
                return Err(Error::ChallengeSuperseded); // nothing sent
            }
            let url = base.join("api/v1/auth/totp")?;
            let resp = http.post(url).json(&body).send().await?;
            if !resp.status().is_success() {
                // Status and code only: the body may echo the submitted code or the token.
                let err = match crate::account::account_error(resp).await {
                    // A malformed code (the server's 422) is, to the user, a wrong code.
                    Error::Api { code, .. } if code == "validation" => Error::Api {
                        code: "auth.invalid_code".into(),
                        message: "the code was refused".into(),
                    },
                    other => other,
                };
                // Back, a sign-out or a newer login meanwhile: that decided, not this refusal.
                if !session.challenge_current(gen).await {
                    return Err(Error::ChallengeSuperseded);
                }
                if matches!(&err, Error::Api { code, .. } if code == "auth.totp_expired") {
                    session.end_challenge(gen).await;
                }
                return Err(err);
            }
            #[derive(serde::Deserialize)]
            struct TotpAnswer {
                access_token: String,
                refresh_token: String,
                recovery_codes_left: Option<u32>,
            }
            let answer: TotpAnswer = resp.json().await.map_err(|_| Error::UnexpectedResponse)?;
            let left = answer.recovery_codes_left;
            let pair = TokenPair {
                access_token: answer.access_token,
                refresh_token: answer.refresh_token,
            };
            let new = session_for(&http, &base, pair).await?;
            if session.install_for_challenge(gen, new.clone()).await {
                Ok(left)
            } else {
                session.revoke_detached(new.refresh_token); // Back or a sign-out won
                Err(Error::ChallengeSuperseded)
            }
        });
        task.await.map_err(|_| Error::UnexpectedResponse)?
    }

    /// The background loops' handles (tests: to see them end).
    #[cfg(test)]
    pub(crate) fn take_tasks(&self) -> Vec<tokio::task::JoinHandle<()>> {
        std::mem::take(&mut *self.tasks.lock().unwrap())
    }

    /// A receiver the UI can watch for [`AuthState`] transitions.
    pub fn state(&self) -> watch::Receiver<AuthState> {
        self.state_rx.clone()
    }

    /// Log in with a local handle + password, publishing state transitions.
    pub async fn login(&self, handle: &str, password: &str) -> Result<LoginOutcome> {
        self.session.note_runtime();
        // `send` only fails if all receivers are dropped; `self` holds `state_rx`,
        // so it can never fail here. Ignoring the result is safe.
        let _ = self.state_tx.send(AuthState::Authenticating);
        // Reserved before waiting for anything: a sign-out (or a newer login) while this one
        // is still queued makes it stale, and a stale login installs nothing.
        let gen = self.session.reserve_login().await;
        let (session, http, base) = (self.session.clone(), self.http.clone(), self.base.clone());
        let (handle, password) = (handle.to_string(), password.to_string());
        // Its own task, holding its own lock: a cancelled caller cannot stop it between the
        // server issuing a pair and core installing (or revoking) it.
        let task = tokio::spawn(async move {
            // The refresh lock: a password change in flight revokes every refresh token of the
            // user when its server call commits; a login in between would install a pair it
            // then revokes. Every holder's requests are bounded by the request timeout.
            let _flight = session.refresh_lock.clone().lock_owned().await;
            // Take any prior session out up front so a failed attempt can never leave the
            // previous user's token usable by chat calls; its refresh token is revoked now,
            // whatever this login's outcome.
            let displaced = session
                .take_out_for_login(gen)
                .await
                .map_err(|()| Error::NotAuthenticated)?;
            if let Some(old) = displaced {
                session.revoke_detached(old.refresh_token);
            }
            let issued = match do_login(&http, &base, &handle, &password).await {
                Ok(issued) => issued,
                Err(err) => {
                    session
                        .publish_failed_if_current(gen, err.to_string())
                        .await;
                    return Err(err);
                }
            };
            match issued {
                Issued::Totp { token, expires_in } => {
                    // Nothing is installed: the password alone never yields a session.
                    if !session.open_challenge(gen).await {
                        return Err(Error::NotAuthenticated); // superseded meanwhile
                    }
                    let expires_at = tokio::time::Instant::now() + Duration::from_secs(expires_in);
                    Ok(LoginOutcome::TotpRequired(TotpChallenge {
                        store: session.id(),
                        gen,
                        token,
                        expires_at,
                    }))
                }
                Issued::Pair(pair) => {
                    let new = match session_for(&http, &base, pair).await {
                        Ok(new) => new,
                        Err(err) => {
                            session
                                .publish_failed_if_current(gen, err.to_string())
                                .await;
                            return Err(err);
                        }
                    };
                    if session.install_for_login(gen, new.clone()).await {
                        Ok(LoginOutcome::LoggedIn(new)) // LoggedIn published by the install
                    } else {
                        session.revoke_detached(new.refresh_token); // superseded meanwhile
                        Err(Error::NotAuthenticated)
                    }
                }
            }
        });
        task.await.map_err(|_| Error::UnexpectedResponse)?
    }

    /// The current access token, or [`Error::NotAuthenticated`] if logged out.
    async fn access_token(&self) -> Result<String> {
        self.session
            .access_token()
            .await
            .ok_or(Error::NotAuthenticated)
    }

    /// The logged-in user's id (for rendering DM titles), or `None` if logged out.
    pub async fn current_user_id(&self) -> Option<String> {
        self.session.with_session(|s| s.user.id.clone()).await
    }

    /// Whether the logged-in user is a global admin.
    pub async fn is_admin(&self) -> bool {
        self.session
            .with_session(|s| s.user.global_role == "admin")
            .await
            .unwrap_or(false)
    }

    /// Channels and DMs the user belongs to.
    pub async fn list_channels(&self) -> Result<Vec<Channel>> {
        let token = self.access_token().await?;
        let url = self.base.join("api/v1/channels")?;
        let resp = self.http.get(url).bearer_auth(token).send().await?;
        self.parse(resp).await
    }

    /// Create a named channel (server requires the caller be a global admin).
    pub async fn create_channel(&self, name: &str, topic: Option<&str>) -> Result<Channel> {
        self.post_channel(json!({ "kind": "channel", "name": name, "topic": topic }))
            .await
    }

    /// Create a public (browsable + self-joinable) channel. Requires admin.
    pub async fn create_public_channel(&self, name: &str) -> Result<Channel> {
        self.post_channel(json!({ "kind": "channel", "name": name, "public": true }))
            .await
    }

    /// Rename / retopic / archive a channel (admin or owner). `None` fields unchanged.
    pub async fn update_channel(
        &self,
        channel_id: &str,
        name: Option<&str>,
        topic: Option<&str>,
        archived: Option<bool>,
    ) -> Result<Channel> {
        let token = self.access_token().await?;
        let url = self.base.join(&format!("api/v1/channels/{channel_id}"))?;
        let resp = self
            .http
            .patch(url)
            .bearer_auth(token)
            .json(&json!({ "name": name, "topic": topic, "archived": archived }))
            .send()
            .await?;
        self.parse(resp).await
    }

    /// Delete a channel and its history (admin or owner).
    pub async fn delete_channel(&self, channel_id: &str) -> Result<()> {
        let token = self.access_token().await?;
        let url = self.base.join(&format!("api/v1/channels/{channel_id}"))?;
        let resp = self.http.delete(url).bearer_auth(token).send().await?;
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        Ok(())
    }

    /// Public, non-archived channels the user hasn't joined yet (to self-join).
    pub async fn list_public_channels(&self) -> Result<Vec<Channel>> {
        let token = self.access_token().await?;
        let url = self.base.join("api/v1/channels/public")?;
        let resp = self.http.get(url).bearer_auth(token).send().await?;
        self.parse(resp).await
    }

    /// Search message bodies across the user's channels (newest first).
    pub async fn search_messages(&self, query: &str) -> Result<Vec<Message>> {
        let token = self.access_token().await?;
        let mut url = self.base.join("api/v1/channels/search")?;
        url.query_pairs_mut().append_pair("q", query);
        let resp = self.http.get(url).bearer_auth(token).send().await?;
        self.parse(resp).await
    }

    /// Signal that we're typing in a channel (ephemeral; debounce on the caller).
    pub async fn send_typing(&self, channel_id: &str) -> Result<()> {
        let token = self.access_token().await?;
        let url = self
            .base
            .join(&format!("api/v1/channels/{channel_id}/typing"))?;
        let resp = self.http.post(url).bearer_auth(token).send().await?;
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        Ok(())
    }

    /// Self-join a public channel.
    pub async fn join_channel(&self, channel_id: &str) -> Result<Channel> {
        let token = self.access_token().await?;
        let url = self
            .base
            .join(&format!("api/v1/channels/{channel_id}/join"))?;
        let resp = self.http.post(url).bearer_auth(token).send().await?;
        self.parse(resp).await
    }

    /// Open (or find the existing) 1:1 DM with the user `member_handle`.
    pub async fn open_dm(&self, member_handle: &str) -> Result<Channel> {
        self.post_channel(json!({ "kind": "dm", "member": member_handle }))
            .await
    }

    async fn post_channel(&self, body: serde_json::Value) -> Result<Channel> {
        let token = self.access_token().await?;
        let url = self.base.join("api/v1/channels")?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await?;
        self.parse(resp).await
    }

    /// Add a member (by handle) to a channel. Requires admin or channel owner.
    pub async fn add_member(&self, channel_id: &str, handle: &str) -> Result<()> {
        let token = self.access_token().await?;
        let url = self
            .base
            .join(&format!("api/v1/channels/{channel_id}/members"))?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(&json!({ "handle": handle }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        Ok(())
    }

    /// Mark a channel read up to `message_id` (or its latest message if `None`).
    pub async fn mark_read(&self, channel_id: &str, message_id: Option<&str>) -> Result<()> {
        let token = self.access_token().await?;
        let url = self
            .base
            .join(&format!("api/v1/channels/{channel_id}/read"))?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(&json!({ "message_id": message_id }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        Ok(())
    }

    /// Channel history, oldest→newest. `before` back-paginates from a message id.
    pub async fn channel_history(
        &self,
        channel_id: &str,
        before: Option<&str>,
    ) -> Result<Vec<Message>> {
        let token = self.access_token().await?;
        let mut url = self
            .base
            .join(&format!("api/v1/channels/{channel_id}/messages"))?;
        if let Some(before) = before {
            url.query_pairs_mut().append_pair("before", before);
        }
        let resp = self.http.get(url).bearer_auth(token).send().await?;
        self.parse(resp).await
    }

    /// Send a message into a channel (the only send path); the server fans it out.
    pub async fn send_message(
        &self,
        channel_id: &str,
        body: &str,
        reply_to_id: Option<&str>,
    ) -> Result<Message> {
        let token = self.access_token().await?;
        let url = self
            .base
            .join(&format!("api/v1/channels/{channel_id}/messages"))?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(&json!({ "body": body, "reply_to_id": reply_to_id }))
            .send()
            .await?;
        self.parse(resp).await
    }

    /// Edit a message's body (author only). Returns the updated message.
    pub async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        body: &str,
    ) -> Result<Message> {
        let token = self.access_token().await?;
        let url = self.base.join(&format!(
            "api/v1/channels/{channel_id}/messages/{message_id}"
        ))?;
        let resp = self
            .http
            .patch(url)
            .bearer_auth(token)
            .json(&json!({ "body": body }))
            .send()
            .await?;
        self.parse(resp).await
    }

    /// Toggle the caller's emoji reaction on a message. Returns the message's full
    /// reaction summary (from the caller's perspective).
    pub async fn toggle_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<Vec<ReactionSummary>> {
        let token = self.access_token().await?;
        let url = self.base.join(&format!(
            "api/v1/channels/{channel_id}/messages/{message_id}/reactions"
        ))?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(&json!({ "emoji": emoji }))
            .send()
            .await?;
        self.parse(resp).await
    }

    /// Delete a message (author or admin).
    pub async fn delete_message(&self, channel_id: &str, message_id: &str) -> Result<()> {
        let token = self.access_token().await?;
        let url = self.base.join(&format!(
            "api/v1/channels/{channel_id}/messages/{message_id}"
        ))?;
        let resp = self.http.delete(url).bearer_auth(token).send().await?;
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        Ok(())
    }

    /// Subscribe to realtime [`ServerEvent`]s (call [`Self::start_realtime`] once
    /// after login to open the socket).
    pub fn events(&self) -> broadcast::Receiver<ServerEvent> {
        self.events_tx.subscribe()
    }

    /// Open the realtime WebSocket (idempotent). Spawns a background reconnect
    /// loop on the current Tokio runtime.
    pub async fn start_realtime(&self) -> Result<()> {
        // Require a session BEFORE claiming the flag, so a call made while logged
        // out fails cleanly and a later (authenticated) call can still start.
        self.access_token().await?;
        let url = ws::ws_url(&self.base)?;
        if self.realtime_started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // The WS reads the current token from the shared session on each (re)connect,
        // and a background loop refreshes the access token before it expires — so a
        // long-lived session keeps both REST calls and the socket authorized.
        let transport = self
            .transport
            .lock()
            .unwrap()
            .take()
            .expect("realtime transport is taken exactly once, guarded by realtime_started");
        let refresher = Refresher {
            http: self.http.clone(),
            base: self.base.clone(),
            session: self.session.clone(),
        };
        // Each loop races its client's shutdown, so it ends from whatever wait it is in.
        // Neither mints anything itself (refreshes run in their own tasks), so cancelling a
        // loop never interrupts a pair between the server and core.
        let until_dropped = |mut shutdown: watch::Receiver<()>| async move {
            let _ = shutdown.changed().await; // Err once the client (the sender) is dropped
        };
        let ws_loop = ws::run(
            url,
            self.session.clone(),
            self.events_tx.clone(),
            transport,
            refresher.clone(),
        );
        let stop = until_dropped(self.shutdown.subscribe());
        let ws_task = tokio::spawn(async move {
            tokio::select! { _ = ws_loop => {}, _ = stop => {} }
        });
        let stop = until_dropped(self.shutdown.subscribe());
        let refresh_task = tokio::spawn(async move {
            tokio::select! { _ = refresh_loop(refresher) => {}, _ = stop => {} }
        });
        self.tasks.lock().unwrap().extend([ws_task, refresh_task]);
        Ok(())
    }

    /// Join the call in `channel_id` (starting it if none is running), driving `engine`.
    /// `publish`: send our mic/camera (false: listen only). Requires the realtime socket to
    /// be connected ([`BrookClient::start_realtime`]); returns once the server confirmed.
    pub async fn join_call(
        &self,
        channel_id: &str,
        engine: Arc<dyn crate::MediaEngine>,
        publish: bool,
    ) -> Result<Arc<crate::CallHandle>> {
        crate::call::join(
            self.commands.clone(),
            self.session.watch(),
            channel_id,
            engine,
            publish,
        )
        .await
    }

    /// Configure the transport before `start_realtime` (tests only).
    #[cfg(test)]
    pub(crate) fn with_transport(&self, f: impl FnOnce(&mut Transport)) {
        f(self
            .transport
            .lock()
            .unwrap()
            .as_mut()
            .expect("before start_realtime"));
    }

    /// Run one refresh now (tests drive the refresh path without waiting for the loop).
    #[cfg(test)]
    pub(crate) async fn refresh_now(&self) -> Result<RefreshOutcome> {
        refresh_once(&self.http, &self.base, &self.session).await
    }

    async fn parse<T: DeserializeOwned>(&self, resp: reqwest::Response) -> Result<T> {
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        resp.json().await.map_err(|_| Error::UnexpectedResponse)
    }
}

#[derive(Deserialize)]
pub(crate) struct TokenPair {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error: Option<ApiErrorContent>,
}

#[derive(Deserialize)]
struct ApiErrorContent {
    code: String,
    message: String,
}

/// Map a non-2xx response into a structured [`Error`].
async fn api_error(resp: reqwest::Response) -> Error {
    let status = resp.status();
    // Redirects are never followed (see `BrookClient::new`), and a redirect's body is not
    // an API error envelope: report the status only, never content the redirecting party
    // chose — it reaches the UI verbatim.
    if status.is_redirection() {
        return Error::Api {
            code: format!("http_{}", status.as_u16()),
            message: format!("unexpected redirect (status {})", status.as_u16()),
        };
    }
    let body = resp.text().await.unwrap_or_else(|err| {
        tracing::warn!(%err, "failed to read error response body");
        String::new()
    });
    if let Ok(ApiErrorEnvelope {
        error: Some(content),
    }) = serde_json::from_str::<ApiErrorEnvelope>(&body)
    {
        return Error::Api {
            code: content.code,
            message: content.message,
        };
    }
    // Non-JSON / unstructured error (e.g. a proxy 502 HTML page): keep the status
    // and a short body snippet so 5xx debugging isn't blind.
    let snippet: String = body.chars().take(120).collect();
    Error::Api {
        code: format!("http_{}", status.as_u16()),
        message: if snippet.trim().is_empty() {
            format!("request failed with status {}", status.as_u16())
        } else {
            snippet
        },
    }
}

/// Password login, `supports_totp` always sent: a pair, or a TOTP step.
async fn do_login(
    http: &reqwest::Client,
    base: &Url,
    handle: &str,
    password: &str,
) -> Result<Issued> {
    let url = base.join("api/v1/auth/login")?;
    let resp = http
        .post(url)
        .json(&json!({ "handle": handle, "password": password, "supports_totp": true }))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(api_error(resp).await);
    }
    #[derive(serde::Deserialize)]
    struct LoginAnswer {
        #[serde(default)]
        totp_required: bool,
        totp_token: Option<String>,
        expires_in: Option<u64>,
        access_token: Option<String>,
        refresh_token: Option<String>,
    }
    let answer: LoginAnswer = resp.json().await.map_err(|_| Error::UnexpectedResponse)?;
    match answer {
        LoginAnswer {
            totp_required: true,
            totp_token: Some(token),
            expires_in,
            ..
        } => Ok(Issued::Totp {
            token,
            expires_in: expires_in.unwrap_or(300),
        }),
        LoginAnswer {
            access_token: Some(access_token),
            refresh_token: Some(refresh_token),
            ..
        } => Ok(Issued::Pair(TokenPair {
            access_token,
            refresh_token,
        })),
        _ => Err(Error::UnexpectedResponse),
    }
}

/// The session a freshly issued pair belongs to. If `/me` fails the pair was issued but will
/// never be held, so it is revoked before the error is returned.
async fn session_for(http: &reqwest::Client, base: &Url, tokens: TokenPair) -> Result<Session> {
    match fetch_me(http, base, &tokens.access_token).await {
        Ok(user) => Ok(Session {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            user,
        }),
        Err(err) => {
            let _ = http
                .post(base.join("api/v1/auth/logout")?)
                .json(&json!({ "refresh_token": tokens.refresh_token }))
                .send()
                .await;
            Err(err)
        }
    }
}

async fn fetch_me(http: &reqwest::Client, base: &Url, access_token: &str) -> Result<User> {
    let url = base.join("api/v1/auth/me")?;
    let resp = http.get(url).bearer_auth(access_token).send().await?;
    if !resp.status().is_success() {
        return Err(api_error(resp).await);
    }
    resp.json().await.map_err(|_| Error::UnexpectedResponse)
}

/// Periodically rotate the access token so a long-lived session keeps REST calls
/// and the socket authorized. While there is no session it idles and polls.
async fn refresh_loop(refresher: Refresher) {
    let mut delay = REFRESH_INTERVAL;
    loop {
        tokio::time::sleep(delay).await;
        let seen = refresher.session.snapshot().await.0;
        let result = refresher.refresh(seen).await;
        if let Err(err) = &result {
            tracing::warn!(%err, "token refresh failed; retrying soon");
        }
        delay = next_refresh_delay(&result);
    }
}

/// When the refresh loop tries again after `result`.
pub(crate) fn next_refresh_delay(result: &Result<RefreshOutcome>) -> Duration {
    match result {
        Ok(RefreshOutcome::Committed) => REFRESH_INTERVAL,
        // The server said when to come back; never sooner than the usual retry.
        Ok(RefreshOutcome::RateLimited(wait)) => (*wait).max(REFRESH_RETRY_INTERVAL),
        // Nothing to do, a newer login won, or the token was rejected (session cleared):
        // poll for the next login.
        Ok(_) => REFRESH_RETRY_INTERVAL,
        Err(_) => REFRESH_RETRY_INTERVAL, // transient (network/5xx) → retry before expiry
    }
}

/// Everything needed to rotate the session's tokens; shared by the periodic loop and the
/// socket's recovery after a 1008 auth close.
#[derive(Clone)]
pub(crate) struct Refresher {
    pub(crate) http: reqwest::Client,
    pub(crate) base: Url,
    pub(crate) session: SessionStore,
}

impl Refresher {
    /// Single-flight refresh. `seen` is the revision the caller considers stale: if another
    /// refresh (or a login) already moved past it while we waited for the lock, nothing is
    /// sent and the caller just uses the current credentials.
    ///
    /// The whole operation, lock included, runs in its own task: a cancelled caller (a dropped
    /// loop, a cancelled account call) leaves it to finish, so the lock is never released
    /// between the server's rotation and core's commit (or revoke).
    pub(crate) async fn refresh(&self, seen: Revision) -> Result<RefreshOutcome> {
        let me = self.clone();
        let task = tokio::spawn(async move {
            let _flight = me.session.refresh_lock.clone().lock_owned().await;
            let now = me.session.snapshot().await.0;
            if now != seen {
                return Ok(if now.epoch == seen.epoch {
                    RefreshOutcome::Committed
                } else {
                    RefreshOutcome::Discarded
                });
            }
            refresh_once(&me.http, &me.base, &me.session).await
        });
        task.await.map_err(|_| Error::UnexpectedResponse)?
    }
}

fn retry_after(resp: &reqwest::Response) -> Duration {
    parse_retry_after(
        resp.headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
    )
}

/// `Retry-After` in whole seconds (the server's form), within 1 s (a 0 would make a zero-wait
/// loop) and an hour (the server's harshest tier; a larger value is not believed). Missing or
/// unreadable (an HTTP date, an overflow): the usual retry interval.
pub(crate) fn parse_retry_after(value: Option<&str>) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.clamp(1, 3600)))
        .unwrap_or(REFRESH_RETRY_INTERVAL)
}

/// What one refresh attempt did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RefreshOutcome {
    /// New credentials are live.
    Committed,
    /// A login/logout replaced the session while we were refreshing; our result was dropped.
    Discarded,
    /// There was no session to refresh.
    NoSession,
    /// The server rejected the refresh token; the session was cleared if it still held it.
    Rejected,
    /// 429: the server asked to wait this long. Not a verdict on the token: the session stays.
    RateLimited(Duration),
}

/// Rotate access+refresh tokens once via `/auth/refresh`. Both the success and the
/// rejection path are compare-and-set against the token we sent, so a slow refresh can
/// never overwrite or erase a session from a newer login.
pub(crate) async fn refresh_once(
    http: &reqwest::Client,
    base: &Url,
    session: &SessionStore,
) -> Result<RefreshOutcome> {
    let Some(refresh_token) = session.with_session(|s| s.refresh_token.clone()).await else {
        return Ok(RefreshOutcome::NoSession);
    };
    // Still inside a 429's wait: do not ask again, whoever the caller is.
    let not_before = *session.refresh_not_before.lock().unwrap();
    if let Some(until) = not_before {
        let now = tokio::time::Instant::now();
        if now < until {
            return Ok(RefreshOutcome::RateLimited(until - now));
        }
    }
    let url = base.join("api/v1/auth/refresh")?;
    let resp = http
        .post(url)
        .json(&json!({ "refresh_token": refresh_token }))
        .send()
        .await?;
    // 429 is "come back later", not "this token is bad": clearing the session here would
    // sign the user out for being rate limited.
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let wait = retry_after(&resp);
        // Only for the session that asked: a 429 arriving after a new sign-in must not make
        // the new session wait.
        if session
            .with_session(|s| s.refresh_token == refresh_token)
            .await
            == Some(true)
        {
            *session.refresh_not_before.lock().unwrap() = Some(tokio::time::Instant::now() + wait);
        }
        return Ok(RefreshOutcome::RateLimited(wait));
    }
    if resp.status().is_client_error() {
        session.clear_if_holds(&refresh_token).await;
        return Ok(RefreshOutcome::Rejected);
    }
    if !resp.status().is_success() {
        return Err(api_error(resp).await); // 5xx → transient
    }
    let tokens: TokenPair = resp.json().await.map_err(|_| Error::UnexpectedResponse)?;
    let fresh = tokens.refresh_token.clone();
    Ok(
        match session
            .commit_refresh(&refresh_token, tokens.access_token, tokens.refresh_token)
            .await
        {
            RefreshApplied::Committed => RefreshOutcome::Committed,
            RefreshApplied::Discarded => {
                session.revoke_detached(fresh); // rotated for a session no longer held
                RefreshOutcome::Discarded
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn client_for(server: &MockServer) -> BrookClient {
        BrookClient::new(CoreConfig::new(&server.uri()).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn login_success_resolves_user_and_state() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "a", "refresh_token": "r", "token_type": "bearer"
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

        let client = client_for(&server).await;
        let mut state = client.state();
        let LoginOutcome::LoggedIn(session) = client.login("alice", "supersecret").await.unwrap()
        else {
            panic!("expected a session");
        };

        assert_eq!(session.user.handle, "alice");
        assert_eq!(session.access_token, "a");
        assert_eq!(
            *state.borrow_and_update(),
            AuthState::LoggedIn(session.user.clone())
        );
    }

    /// A redirect must never be followed: the https/loopback rule in `CoreConfig` only
    /// checks the configured URL, and reqwest re-sends a 307/308 request *with its body* —
    /// the password — to wherever `Location` points, including plain http elsewhere.
    #[tokio::test]
    async fn login_does_not_follow_redirects_or_resend_credentials() {
        let elsewhere = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&elsewhere)
            .await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/login"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("Location", format!("{}/steal", elsewhere.uri()).as_str()),
            )
            .mount(&server)
            .await;

        let client = client_for(&server).await;
        let err = client.login("alice", "supersecret").await.unwrap_err();

        assert!(
            matches!(&err, Error::Api { code, .. } if code == "http_307"),
            "expected the 307 to surface as an error, got {err:?}"
        );
        assert!(
            elsewhere.received_requests().await.unwrap().is_empty(),
            "credentials were re-sent to the redirect target"
        );
    }

    /// A redirect's body is not an API error and must not be echoed to the UI: it could
    /// carry anything the redirecting party chose, including reflected secrets.
    #[tokio::test]
    async fn redirect_error_does_not_echo_the_response_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/login"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("Location", "http://elsewhere.invalid/")
                    .set_body_json(json!({
                        "error": { "code": "echo", "message": "supersecret" }
                    })),
            )
            .mount(&server)
            .await;

        let client = client_for(&server).await;
        let err = client.login("alice", "supersecret").await.unwrap_err();

        assert!(!err.to_string().contains("supersecret"), "echoed: {err}");
        assert!(
            matches!(&err, Error::Api { code, .. } if code == "http_307"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn login_rejected_maps_api_error_and_state() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/login"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": { "code": "auth.invalid_credentials", "message": "Invalid handle or password" }
            })))
            .mount(&server)
            .await;

        let client = client_for(&server).await;
        let err = client.login("alice", "wrong").await.unwrap_err();

        match err {
            Error::Api { code, .. } => assert_eq!(code, "auth.invalid_credentials"),
            other => panic!("expected Api error, got {other:?}"),
        }
        assert!(matches!(*client.state().borrow(), AuthState::Failed(_)));
    }

    async fn logged_in_client(server: &MockServer) -> BrookClient {
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "a", "refresh_token": "r", "token_type": "bearer"
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/auth/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "u1", "handle": "alice", "display_name": "Alice", "global_role": "admin"
            })))
            .mount(server)
            .await;
        let client = client_for(server).await;
        client.login("alice", "supersecret").await.unwrap();
        client
    }

    #[tokio::test]
    async fn lists_channels_and_titles_dm_by_other_member() {
        let server = MockServer::start().await;
        let client = logged_in_client(&server).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/channels"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
                "id": "c1", "kind": "dm", "name": null, "topic": null,
                "created_by": "u1", "created_at": "2026-06-18T00:00:00Z",
                "members": [
                    {"id": "u1", "handle": "alice", "display_name": "Alice"},
                    {"id": "u2", "handle": "bob", "display_name": "Bob"}
                ]
            }])))
            .mount(&server)
            .await;

        let channels = client.list_channels().await.unwrap();
        assert_eq!(channels.len(), 1);
        assert!(channels[0].is_dm());
        // A DM is titled by the *other* member.
        assert_eq!(channels[0].title("u1"), "Bob");
    }

    #[tokio::test]
    async fn sends_message_and_parses_author() {
        let server = MockServer::start().await;
        let client = logged_in_client(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/channels/c1/messages"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "019ed8", "channel_id": "c1", "author_id": "u1",
                "author_handle": "alice", "author_display_name": "Alice",
                "body": "hi bob", "created_at": "2026-06-18T00:00:00Z", "edited_at": null
            })))
            .mount(&server)
            .await;

        let message = client.send_message("c1", "hi bob", None).await.unwrap();
        assert_eq!(message.body, "hi bob");
        assert_eq!(message.author_handle.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn chat_calls_require_login() {
        let server = MockServer::start().await;
        let client = client_for(&server).await;
        assert!(matches!(
            client.list_channels().await.unwrap_err(),
            Error::NotAuthenticated
        ));
    }
}
