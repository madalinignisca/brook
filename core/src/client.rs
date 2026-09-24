//! The Brook core client.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{broadcast, watch};
use url::Url;

use crate::session_store::{RefreshApplied, SessionStore};
use crate::ws::{self, Commands, ServerEvent, Transport};
use crate::{
    AuthState, Channel, CoreConfig, Error, Message, ReactionSummary, Result, Session, User,
};

/// Refresh the access token this long before its ~15 min server TTL elapses.
const REFRESH_INTERVAL: Duration = Duration::from_secs(600);

/// After a transient refresh failure (or while logged out), poll again this soon
/// — short enough to recover well before the access token expires.
const REFRESH_RETRY_INTERVAL: Duration = Duration::from_secs(15);

/// Shared client: networking + observable auth state.
///
/// Cheap to clone-by-`Arc` from the UI; safe to call from any async task.
pub struct BrookClient {
    base: Url,
    http: reqwest::Client,
    state_tx: Arc<watch::Sender<AuthState>>,
    state_rx: watch::Receiver<AuthState>,
    /// The active session (set on login), used to authorize chat calls + the WS.
    session: SessionStore,
    /// Realtime events fan-out to UI subscribers.
    events_tx: broadcast::Sender<ServerEvent>,
    /// Guards against starting the realtime task more than once.
    realtime_started: AtomicBool,
    /// Sends commands over the realtime socket (calls).
    #[cfg_attr(not(test), allow(dead_code))] // used by the call layer (C1b P4)
    pub(crate) commands: Commands,
    /// The socket side of `commands`, handed to the realtime task when it starts.
    transport: std::sync::Mutex<Option<Transport>>,
}

impl BrookClient {
    /// Create a client for the given configuration.
    pub fn new(config: CoreConfig) -> Result<Self> {
        let http = reqwest::Client::builder().build()?;
        let (state_tx, state_rx) = watch::channel(AuthState::LoggedOut);
        let state_tx = Arc::new(state_tx);
        let (commands, transport) = ws::command_channel();
        let (events_tx, _) = broadcast::channel(256);
        Ok(Self {
            base: config.base_url,
            http,
            session: SessionStore::new(state_tx.clone()),
            state_tx,
            state_rx,
            events_tx,
            realtime_started: AtomicBool::new(false),
            commands,
            transport: std::sync::Mutex::new(Some(transport)),
        })
    }

    /// A receiver the UI can watch for [`AuthState`] transitions.
    pub fn state(&self) -> watch::Receiver<AuthState> {
        self.state_rx.clone()
    }

    /// Log in with a local handle + password, publishing state transitions.
    pub async fn login(&self, handle: &str, password: &str) -> Result<Session> {
        // `send` only fails if all receivers are dropped; `self` holds `state_rx`,
        // so it can never fail here. Ignoring the result is safe.
        let _ = self.state_tx.send(AuthState::Authenticating);
        // Drop any prior session up front so a failed attempt can never leave the
        // previous user's token usable by chat calls.
        self.session.replace(None).await;
        let result = self.do_login(handle, password).await;
        let next = match &result {
            Ok(session) => AuthState::LoggedIn(session.user.clone()),
            Err(err) => AuthState::Failed(err.to_string()),
        };
        let _ = self.state_tx.send(next);
        result
    }

    async fn do_login(&self, handle: &str, password: &str) -> Result<Session> {
        let url = self.base.join("api/v1/auth/login")?;
        let resp = self
            .http
            .post(url)
            .json(&json!({ "handle": handle, "password": password }))
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        let tokens: TokenPair = resp.json().await.map_err(|_| Error::UnexpectedResponse)?;
        let user = self.fetch_me(&tokens.access_token).await?;
        let session = Session {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            user,
        };
        // Retain the session so chat calls and the WS can authorize.
        self.session.replace(Some(session.clone())).await;
        Ok(session)
    }

    async fn fetch_me(&self, access_token: &str) -> Result<User> {
        let url = self.base.join("api/v1/auth/me")?;
        let resp = self.http.get(url).bearer_auth(access_token).send().await?;
        if !resp.status().is_success() {
            return Err(api_error(resp).await);
        }
        resp.json().await.map_err(|_| Error::UnexpectedResponse)
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
        tokio::spawn(ws::run(
            url,
            self.session.clone(),
            self.events_tx.clone(),
            transport,
        ));
        tokio::spawn(refresh_loop(
            self.http.clone(),
            self.base.clone(),
            self.session.clone(),
        ));
        Ok(())
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
struct TokenPair {
    access_token: String,
    refresh_token: String,
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

/// Periodically rotate the access token so a long-lived session keeps REST calls
/// and the WebSocket authorized. Runs for the client's life: when there's no
/// session (logged out, or the refresh token was rejected) it idles and polls, so
/// Periodically rotate the access token so a long-lived session keeps REST calls
/// and the socket authorized. While there is no session it idles and polls.
async fn refresh_loop(http: reqwest::Client, base: Url, session: SessionStore) {
    let mut delay = REFRESH_INTERVAL;
    loop {
        tokio::time::sleep(delay).await;
        delay = match refresh_once(&http, &base, &session).await {
            Ok(RefreshOutcome::Committed) => REFRESH_INTERVAL,
            // Nothing to do, a newer login won, or the token was rejected (session cleared):
            // poll for the next login.
            Ok(_) => REFRESH_RETRY_INTERVAL,
            Err(err) => {
                tracing::warn!(%err, "token refresh failed; retrying soon");
                REFRESH_RETRY_INTERVAL // transient (network/5xx) → retry before expiry
            }
        };
    }
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
    let url = base.join("api/v1/auth/refresh")?;
    let resp = http
        .post(url)
        .json(&json!({ "refresh_token": refresh_token }))
        .send()
        .await?;
    if resp.status().is_client_error() {
        session.clear_if_holds(&refresh_token).await;
        return Ok(RefreshOutcome::Rejected);
    }
    if !resp.status().is_success() {
        return Err(api_error(resp).await); // 5xx → transient
    }
    let tokens: TokenPair = resp.json().await.map_err(|_| Error::UnexpectedResponse)?;
    Ok(
        match session
            .commit_refresh(&refresh_token, tokens.access_token, tokens.refresh_token)
            .await
        {
            RefreshApplied::Committed => RefreshOutcome::Committed,
            RefreshApplied::Discarded => RefreshOutcome::Discarded,
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
        let session = client.login("alice", "supersecret").await.unwrap();

        assert_eq!(session.user.handle, "alice");
        assert_eq!(session.access_token, "a");
        assert_eq!(
            *state.borrow_and_update(),
            AuthState::LoggedIn(session.user.clone())
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
