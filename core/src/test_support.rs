//! Test harness: one in-process origin serving the REST auth endpoints and the `/ws`
//! upgrade (the client derives both from a single base URL), with each accepted socket
//! handed to the test to drive imperatively — so tests choose frame order exactly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Semaphore};

use crate::{BrookClient, CoreConfig};

/// How `/auth/refresh` answers.
#[derive(Clone, Copy, Debug)]
pub enum RefreshMode {
    /// Issue a fresh token pair, whatever token was sent.
    Rotate,
    /// Like the real server: only a live refresh token rotates (and is consumed); an unknown or
    /// revoked one is answered 401 `auth.invalid_token`.
    Strict,
    /// Answer with this status and an error envelope.
    Fail(u16),
    /// 429 `auth.rate_limited` with `Retry-After: <seconds>`.
    RateLimited(u32),
    /// Never answer (a stalled request).
    Stall,
}

/// How the password endpoints answer (`/auth/password`, `/users/{id}/password`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasswordMode {
    /// Commit: revoke the user's refresh tokens (and, for a self change, issue a fresh pair).
    Ok,
    /// 403 `auth.invalid_credentials` (wrong current password).
    WrongCurrent,
    /// 422 whose body echoes the submitted passwords, as FastAPI's validation errors do.
    Echo422,
    /// 403 `authz.forbidden` (not an admin).
    Forbidden,
    /// 400 `invalid` (an admin targeting their own id).
    SelfTarget,
    /// 404 `not_found`.
    NotFound,
    /// A server from before `sign_out_other_devices`: ignores it, always revokes the refresh
    /// tokens (never the access tokens), and answers a plain TokenPair.
    Legacy,
}

struct ServerState {
    next: u32,
    /// access token → user handle
    tokens: HashMap<String, String>,
    /// live refresh token → user handle (`Strict` refresh and the password endpoints use it)
    refresh_tokens: HashMap<String, String>,
    refresh_mode: RefreshMode,
    refresh_calls: u32,
    stall_login: bool,
    password_mode: PasswordMode,
    /// Answer this many authenticated calls (password, users) with 401 first.
    expire_next: u32,
    /// Held after the server-side commit of a password call, before the response is sent.
    password_gate: Option<Arc<Semaphore>>,
    /// Held before a scripted 401 on the user list is sent (a response still in flight).
    expired_gate: Option<Arc<Semaphore>>,
    /// Every password/users request: (path, bearer token, JSON body).
    requests: Vec<(String, String, Value)>,
    sockets: mpsc::UnboundedSender<WsPeer>,
}

type Shared = Arc<Mutex<ServerState>>;

/// A running test origin.
pub struct TestServer {
    pub base: String,
    state: Shared,
    sockets: mpsc::UnboundedReceiver<WsPeer>,
}

impl TestServer {
    pub async fn start() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(ServerState {
            next: 0,
            tokens: HashMap::new(),
            refresh_tokens: HashMap::new(),
            refresh_mode: RefreshMode::Rotate,
            refresh_calls: 0,
            stall_login: false,
            password_mode: PasswordMode::Ok,
            expire_next: 0,
            password_gate: None,
            expired_gate: None,
            requests: Vec::new(),
            sockets: tx,
        }));
        let app = Router::new()
            .route("/api/v1/auth/login", post(login))
            .route("/api/v1/auth/me", get(me))
            .route("/api/v1/auth/refresh", post(refresh))
            .route("/api/v1/auth/password", post(change_password))
            .route("/api/v1/users", get(list_users))
            .route("/api/v1/users/{id}/password", post(reset_password))
            .route("/ws", get(ws_upgrade))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base: format!("http://{addr}"),
            state,
            sockets: rx,
        }
    }

    /// A client pointed at this origin (loopback http is allowed by core).
    pub fn client(&self) -> BrookClient {
        BrookClient::new(CoreConfig::new(&self.base).unwrap()).unwrap()
    }

    pub fn set_refresh_mode(&self, mode: RefreshMode) {
        self.state.lock().unwrap().refresh_mode = mode;
    }

    pub fn refresh_calls(&self) -> u32 {
        self.state.lock().unwrap().refresh_calls
    }

    pub fn set_stall_login(&self, stall: bool) {
        self.state.lock().unwrap().stall_login = stall;
    }

    pub fn set_password_mode(&self, mode: PasswordMode) {
        self.state.lock().unwrap().password_mode = mode;
    }

    /// Answer the next `n` authenticated calls (password, users) with 401.
    pub fn expire_next(&self, n: u32) {
        self.state.lock().unwrap().expire_next = n;
    }

    /// Hold password responses after the server-side commit until the returned gate gets a
    /// permit.
    pub fn gate_password(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        self.state.lock().unwrap().password_gate = Some(gate.clone());
        gate
    }

    /// Hold the user list's scripted 401 (see `expire_next`) until the returned gate gets a
    /// permit, so a test can change the session while that answer is in flight.
    pub fn gate_expired(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        self.state.lock().unwrap().expired_gate = Some(gate.clone());
        gate
    }

    /// Every password/users request so far: (path, bearer token, JSON body).
    pub fn requests(&self) -> Vec<(String, String, Value)> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Whether this refresh token is still live (not rotated, not revoked).
    pub fn refresh_token_live(&self, token: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .refresh_tokens
            .contains_key(token)
    }

    /// The next socket a client opens (bounded wait).
    pub async fn accept(&mut self) -> WsPeer {
        tokio::time::timeout(Duration::from_secs(10), self.sockets.recv())
            .await
            .expect("client never connected")
            .expect("server gone")
    }
}

fn issue(state: &mut ServerState, handle: &str) -> Value {
    state.next += 1;
    let access = format!("access-{}", state.next);
    let refresh = format!("refresh-{}", state.next);
    state.tokens.insert(access.clone(), handle.to_string());
    state
        .refresh_tokens
        .insert(refresh.clone(), handle.to_string());
    json!({ "access_token": access, "refresh_token": refresh, "token_type": "bearer" })
}

fn error(status: u16, code: &str, message: &str) -> Response {
    (
        StatusCode::from_u16(status).unwrap(),
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

fn bearer(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string()
}

/// Record the request, then the common 401 cases: a scripted expiry, or an unknown token.
/// On success, the caller's handle. (A test fake: the response is returned as is, unboxed.)
#[allow(clippy::result_large_err)]
fn authed(
    state: &mut ServerState,
    path: &str,
    token: &str,
    body: Value,
) -> Result<String, Response> {
    state
        .requests
        .push((path.to_string(), token.to_string(), body));
    if state.expire_next > 0 {
        state.expire_next -= 1;
        return Err(error(401, "auth.token_expired", "access token expired"));
    }
    state
        .tokens
        .get(token)
        .cloned()
        .ok_or_else(|| error(401, "auth.invalid_token", "unknown access token"))
}

fn revoke_refresh_tokens(state: &mut ServerState, handle: &str) {
    state.refresh_tokens.retain(|_, h| h != handle);
}

/// Held after the commit, when the test asked to hold responses.
async fn after_commit(gate: Option<Arc<Semaphore>>) {
    if let Some(gate) = gate {
        gate.acquire().await.unwrap().forget();
    }
}

fn echo_422(body: &Value) -> Response {
    // FastAPI validation errors echo the submitted value ("input").
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({ "detail": [{ "loc": ["body", "new_password"],
            "msg": "String should have at least 8 characters",
            "input": body["new_password"], "ctx": { "current": body["current_password"] } }] })),
    )
        .into_response()
}

async fn login(State(state): State<Shared>, Json(body): Json<Value>) -> Response {
    let handle = body["handle"].as_str().unwrap_or_default().to_string();
    let stall = state.lock().unwrap().stall_login;
    if stall {
        std::future::pending::<()>().await;
    }
    Json(issue(&mut state.lock().unwrap(), &handle)).into_response()
}

async fn change_password(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let token = bearer(&headers);
    let (pair, gate) = {
        let mut state = state.lock().unwrap();
        let handle = match authed(&mut state, "/auth/password", &token, body.clone()) {
            Ok(h) => h,
            Err(r) => return r,
        };
        match state.password_mode {
            PasswordMode::WrongCurrent => {
                return error(403, "auth.invalid_credentials", "wrong password")
            }
            PasswordMode::Echo422 => return echo_422(&body),
            _ => {}
        }
        let legacy = state.password_mode == PasswordMode::Legacy;
        let sign_out = legacy || body["sign_out_other_devices"] != false;
        if sign_out {
            revoke_refresh_tokens(&mut state, &handle);
        }
        if sign_out && !legacy {
            // Server PR #45: every access token issued before the change is refused at once.
            state.tokens.retain(|_, h| *h != handle);
        }
        let mut pair = issue(&mut state, &handle);
        if !legacy {
            pair["other_devices_signed_out"] = json!(sign_out); // what the server did
        }
        (pair, state.password_gate.clone())
    };
    after_commit(gate).await;
    Json(pair).into_response()
}

async fn reset_password(
    State(state): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let token = bearer(&headers);
    let gate = {
        let mut state = state.lock().unwrap();
        let path = format!("/users/{id}/password");
        if let Err(r) = authed(&mut state, &path, &token, body.clone()) {
            return r;
        }
        match state.password_mode {
            PasswordMode::WrongCurrent => {
                return error(403, "auth.invalid_credentials", "wrong admin password")
            }
            PasswordMode::Forbidden => return error(403, "authz.forbidden", "admins only"),
            PasswordMode::SelfTarget => return error(400, "invalid", "use /auth/password"),
            PasswordMode::NotFound => return error(404, "not_found", "no such user"),
            PasswordMode::Echo422 => return echo_422(&body),
            _ => {}
        }
        let handle = id.strip_prefix("id-").unwrap_or(&id).to_string();
        revoke_refresh_tokens(&mut state, &handle);
        state.password_gate.clone()
    };
    after_commit(gate).await;
    StatusCode::NO_CONTENT.into_response()
}

async fn list_users(State(state): State<Shared>, headers: HeaderMap, uri: Uri) -> Response {
    let token = bearer(&headers);
    let held = {
        let mut state = state.lock().unwrap();
        match authed(&mut state, "/users", &token, Value::Null) {
            Ok(_) => None,
            Err(r) => Some((r, state.expired_gate.clone())),
        }
    };
    if let Some((r, gate)) = held {
        after_commit(gate).await; // the same hold, used before an answer that commits nothing
        return r;
    }
    let state = state.lock().unwrap();
    if state.password_mode == PasswordMode::Forbidden {
        return error(403, "authz.forbidden", "admins only");
    }
    let mut handles: Vec<String> = state.tokens.values().cloned().collect();
    handles.sort();
    handles.dedup();
    let wanted = uri
        .query()
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("handle=")))
        .map(str::to_string);
    if let Some(wanted) = wanted {
        handles.retain(|h| *h == wanted);
        if handles.is_empty() {
            return error(404, "not_found", "no such user");
        }
    }
    Json(
        handles
            .iter()
            .map(|h| json!({ "id": format!("id-{h}"), "handle": h, "display_name": h, "global_role": "member" }))
            .collect::<Vec<_>>(),
    )
    .into_response()
}

async fn me(State(state): State<Shared>, headers: HeaderMap) -> Response {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    match state.lock().unwrap().tokens.get(token) {
        Some(handle) => Json(json!({
            "id": format!("id-{handle}"), "handle": handle,
            "display_name": handle, "global_role": "member"
        }))
        .into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn refresh(State(state): State<Shared>, Json(body): Json<Value>) -> Response {
    let mode = {
        let mut state = state.lock().unwrap();
        state.refresh_calls += 1;
        state.refresh_mode
    };
    if let RefreshMode::Stall = mode {
        std::future::pending::<()>().await;
    }
    let mut state = state.lock().unwrap();
    match mode {
        RefreshMode::Stall => unreachable!(),
        RefreshMode::Rotate => Json(issue(&mut state, "alice")).into_response(),
        RefreshMode::Strict => {
            let sent = body["refresh_token"].as_str().unwrap_or_default();
            match state.refresh_tokens.remove(sent) {
                Some(handle) => Json(issue(&mut state, &handle)).into_response(),
                None => error(
                    401,
                    "auth.invalid_token",
                    "refresh token revoked or unknown",
                ),
            }
        }
        RefreshMode::RateLimited(secs) => (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", secs.to_string())],
            Json(json!({ "error": { "code": "auth.rate_limited", "message": "slow down" } })),
        )
            .into_response(),
        RefreshMode::Fail(code) => (
            StatusCode::from_u16(code).unwrap(),
            Json(
                json!({ "error": { "code": "auth.invalid_token", "message": "refresh rejected" } }),
            ),
        )
            .into_response(),
    }
}

async fn ws_upgrade(State(state): State<Shared>, upgrade: WebSocketUpgrade) -> Response {
    let sockets = state.lock().unwrap().sockets.clone();
    upgrade.on_upgrade(move |socket| async move {
        let _ = sockets.send(WsPeer { socket });
    })
}

/// The server end of one client socket, driven by the test.
pub struct WsPeer {
    socket: WebSocket,
}

impl WsPeer {
    /// Next JSON frame from the client (bounded wait; panics on close or timeout).
    pub async fn recv(&mut self) -> Value {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), self.socket.recv())
                .await
                .expect("client sent nothing")
                .expect("socket closed")
                .expect("socket error");
            match msg {
                AxMessage::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
                AxMessage::Close(_) => panic!("client closed the socket"),
                _ => continue,
            }
        }
    }

    pub async fn send(&mut self, frame: Value) {
        self.socket
            .send(AxMessage::Text(frame.to_string().into()))
            .await
            .unwrap();
    }

    /// Standard handshake: expect the `auth` frame, answer `ready` (echoing `re` when present).
    pub async fn accept_auth(&mut self) -> Value {
        let auth = self.recv().await;
        assert_eq!(auth["type"], "auth", "first frame must be auth: {auth}");
        let mut ready = json!({ "type": "ready", "data": { "user_id": "id-alice" } });
        if let Some(id) = auth.get("id") {
            ready["re"] = id.clone();
        }
        self.send(ready).await;
        auth
    }

    /// Wait until the client closes this socket (bounded); panics if it sends a text frame.
    pub async fn expect_closed(&mut self) {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), self.socket.recv())
                .await
                .expect("client kept the socket open")
            {
                None | Some(Err(_)) | Some(Ok(AxMessage::Close(_))) => return,
                Some(Ok(AxMessage::Text(t))) => panic!("expected close, got frame {t}"),
                Some(Ok(_)) => continue,
            }
        }
    }

    pub async fn close(&mut self, code: u16, reason: &str) {
        let _ = self
            .socket
            .send(AxMessage::Close(Some(CloseFrame {
                code,
                reason: reason.to_string().into(),
            })))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::RefreshOutcome;
    use crate::{AuthState, ServerEvent};

    /// P0 smoke: through the public client — login, realtime start, auth frame carries
    /// the issued token, `ready` becomes `ServerEvent::Ready`, server close is survived.
    #[tokio::test]
    async fn smoke_login_connect_ready_disconnect() {
        let mut server = TestServer::start().await;
        let client = server.client();
        client.login("alice", "pw").await.unwrap();
        let mut events = client.events();
        client.start_realtime().await.unwrap();

        let mut peer = server.accept().await;
        let auth = peer.accept_auth().await;
        assert_eq!(auth["data"]["access_token"], "access-1");
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event, ServerEvent::Ready);

        peer.close(1000, "bye").await;
        // The client reconnects after a backoff: a second socket arrives and authenticates again.
        let mut again = server.accept().await;
        again.accept_auth().await;
    }

    /// Signing in as someone else must not leave the socket authenticated as the previous
    /// user: it closes, and the next socket authenticates with the new user's token.
    #[tokio::test]
    async fn login_as_another_user_replaces_the_socket_identity() {
        let mut server = TestServer::start().await;
        let client = server.client();
        client.login("alice", "pw").await.unwrap();
        client.start_realtime().await.unwrap();
        let mut alice = server.accept().await;
        assert_eq!(
            alice.accept_auth().await["data"]["access_token"],
            "access-1"
        );

        client.login("bob", "pw").await.unwrap();
        alice.expect_closed().await;
        let mut bob = server.accept().await;
        assert_eq!(bob.accept_auth().await["data"]["access_token"], "access-2");
    }

    /// A refresh the server rejects clears the session and tells the UI (LoggedOut).
    #[tokio::test]
    async fn rejected_refresh_publishes_logged_out() {
        let server = TestServer::start().await;
        let client = server.client();
        client.login("alice", "pw").await.unwrap();
        server.set_refresh_mode(RefreshMode::Fail(401));
        let outcome = client.refresh_now().await.unwrap();
        assert_eq!(outcome, crate::client::RefreshOutcome::Rejected);
        assert_eq!(*client.state().borrow(), AuthState::LoggedOut);
        assert!(client.current_user_id().await.is_none());
        assert_eq!(server.refresh_calls(), 1);
    }

    // ---- P2: command path ----

    use crate::ws::{CallFrame, CommandError};
    use tokio::sync::mpsc;

    /// Logged in, realtime started, first socket authenticated; returns the client, the
    /// server end, and the ready generation.
    async fn connected(
        server: &mut TestServer,
        configure: impl FnOnce(&mut crate::ws::Transport),
    ) -> (Arc<BrookClient>, WsPeer, u64) {
        let client = Arc::new(server.client());
        client.login("alice", "pw").await.unwrap();
        client.with_transport(configure);
        client.start_realtime().await.unwrap();
        let mut peer = server.accept().await;
        peer.accept_auth().await;
        let mut conn = client.commands.conn();
        let conn = conn.wait_for(|c| c.ready).await.unwrap();
        let generation = conn.generation;
        (client, peer, generation)
    }

    fn cmd(ty: &str) -> Value {
        json!({ "type": ty, "data": {} })
    }

    #[tokio::test]
    async fn replies_are_matched_by_re_not_by_order() {
        let mut server = TestServer::start().await;
        let (client, mut peer, generation) = connected(&mut server, |_| {}).await;
        let c1 = client.clone();
        let first = tokio::spawn(async move {
            c1.commands
                .request(generation, cmd("call.media"), "call.ok", None)
                .await
        });
        let a = peer.recv().await;
        let c2 = client.clone();
        let second = tokio::spawn(async move {
            c2.commands
                .request(generation, cmd("call.leave"), "call.ok", None)
                .await
        });
        let b = peer.recv().await;
        peer.send(json!({ "type": "call.ok", "re": b["id"], "data": { "which": "second" } }))
            .await;
        peer.send(json!({ "type": "call.ok", "re": a["id"], "data": { "which": "first" } }))
            .await;
        assert_eq!(first.await.unwrap().unwrap().data["which"], "first");
        assert_eq!(second.await.unwrap().unwrap().data["which"], "second");
    }

    #[tokio::test]
    async fn reply_of_the_wrong_type_is_rejected() {
        let mut server = TestServer::start().await;
        let (client, mut peer, generation) = connected(&mut server, |_| {}).await;
        let c = client.clone();
        let req = tokio::spawn(async move {
            c.commands
                .request(generation, cmd("call.join"), "call.joined", None)
                .await
        });
        let f = peer.recv().await;
        peer.send(json!({ "type": "call.ok", "re": f["id"], "data": {} }))
            .await;
        assert_eq!(req.await.unwrap(), Err(CommandError::UnexpectedReply));
    }

    #[tokio::test]
    async fn error_reply_carries_the_server_code() {
        let mut server = TestServer::start().await;
        let (client, mut peer, generation) = connected(&mut server, |_| {}).await;
        let c = client.clone();
        let req = tokio::spawn(async move {
            c.commands
                .request(generation, cmd("call.join"), "call.joined", None)
                .await
        });
        let f = peer.recv().await;
        peer.send(json!({ "type": "error", "re": f["id"], "data": { "code": "call_full", "message": "x" } })).await;
        assert_eq!(
            req.await.unwrap(),
            Err(CommandError::Rejected {
                code: "call_full".into(),
                message: "x".into()
            })
        );
    }

    #[tokio::test]
    async fn stale_generation_is_not_sent_and_nothing_leaks_to_the_next_socket() {
        let mut server = TestServer::start().await;
        let (client, mut peer, generation) = connected(&mut server, |_| {}).await;
        peer.close(1000, "drop").await;
        let mut conn = client.commands.conn();
        conn.wait_for(|c| !c.ready).await.unwrap();
        // Submitted while disconnected: fails immediately, never queued.
        let err = client
            .commands
            .request(generation, cmd("call.media"), "call.ok", None)
            .await;
        assert_eq!(err, Err(CommandError::NotSent));
        let mut next = server.accept().await;
        next.accept_auth().await;
        conn.wait_for(|c| c.ready).await.unwrap();
        // The new socket receives our next command, and nothing before it.
        let new_gen = conn.borrow().generation;
        assert!(new_gen > generation);
        let c = client.clone();
        tokio::spawn(async move {
            c.commands
                .request(new_gen, cmd("call.leave"), "call.ok", None)
                .await
        });
        assert_eq!(next.recv().await["type"], "call.leave");
    }

    #[tokio::test]
    async fn socket_drop_with_a_command_in_flight_is_unknown() {
        let mut server = TestServer::start().await;
        let (client, mut peer, generation) = connected(&mut server, |_| {}).await;
        let c = client.clone();
        let req = tokio::spawn(async move {
            c.commands
                .request(generation, cmd("call.media"), "call.ok", None)
                .await
        });
        peer.recv().await; // written
        peer.close(1000, "drop").await;
        let res = tokio::time::timeout(Duration::from_secs(5), req)
            .await
            .expect("pending leaked");
        assert_eq!(res.unwrap(), Err(CommandError::Unknown));
    }

    #[tokio::test]
    async fn no_reply_times_out() {
        let mut server = TestServer::start().await;
        let (client, mut peer, generation) = connected(&mut server, |t| {
            t.reply_timeout = Duration::from_millis(300)
        })
        .await;
        let c = client.clone();
        let req = tokio::spawn(async move {
            c.commands
                .request(generation, cmd("call.media"), "call.ok", None)
                .await
        });
        peer.recv().await;
        let res = tokio::time::timeout(Duration::from_secs(5), req)
            .await
            .expect("never timed out");
        assert_eq!(res.unwrap(), Err(CommandError::Timeout));
    }

    /// The reply window starts when the frame is written, not when it was submitted: a
    /// command held before the write for longer than the timeout still gets a full window.
    #[tokio::test]
    async fn timeout_counts_from_the_write() {
        let mut server = TestServer::start().await;
        let hold = Arc::new(tokio::sync::Semaphore::new(0));
        let h = hold.clone();
        let (client, mut peer, generation) = connected(&mut server, move |t| {
            t.reply_timeout = Duration::from_millis(400);
            t.hold_writes = Some(h);
        })
        .await;
        let c = client.clone();
        let req = tokio::spawn(async move {
            c.commands
                .request(generation, cmd("call.media"), "call.ok", None)
                .await
        });
        tokio::time::sleep(Duration::from_millis(700)).await; // longer than the timeout
        hold.add_permits(1);
        let f = peer.recv().await;
        tokio::time::sleep(Duration::from_millis(100)).await; // well inside a window from the write
        peer.send(json!({ "type": "call.ok", "re": f["id"], "data": {} }))
            .await;
        assert!(req.await.unwrap().is_ok());
    }

    /// A command accepted on socket A but held before its write, then released only after
    /// socket B is ready, must not be written on B.
    #[tokio::test]
    async fn command_held_across_a_reconnect_is_never_written() {
        let mut server = TestServer::start().await;
        let hold = Arc::new(tokio::sync::Semaphore::new(0));
        let h = hold.clone();
        let (client, mut peer, generation) =
            connected(&mut server, move |t| t.hold_writes = Some(h)).await;
        let c = client.clone();
        let req = tokio::spawn(async move {
            c.commands
                .request(generation, cmd("call.media"), "call.ok", None)
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await; // accepted, held before the write
        peer.close(1000, "drop").await;
        let mut next = server.accept().await;
        next.accept_auth().await;
        let mut conn = client.commands.conn();
        conn.wait_for(|c| c.ready && c.generation > generation)
            .await
            .unwrap();
        hold.add_permits(100); // release only now that B is the current socket
        let res = tokio::time::timeout(Duration::from_secs(5), req)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(res, Err(CommandError::NotSent));
        let new_gen = conn.borrow().generation;
        let c = client.clone();
        tokio::spawn(async move {
            c.commands
                .request(new_gen, cmd("call.leave"), "call.ok", None)
                .await
        });
        assert_eq!(
            next.recv().await["type"],
            "call.leave",
            "the held command leaked to the new socket"
        );
    }

    /// `call.joined` and the first offer back-to-back while the call task is not reading:
    /// the transport installed the route while handling `call.joined`, so the offer is
    /// waiting in the task's mailbox, right after `call.joined`.
    #[tokio::test]
    async fn offer_right_behind_call_joined_reaches_the_call_task() {
        let mut server = TestServer::start().await;
        let (client, mut peer, generation) = connected(&mut server, |_| {}).await;
        let (route, mut mailbox) = mpsc::unbounded_channel::<CallFrame>();
        let c = client.clone();
        let join = tokio::spawn(async move {
            c.commands
                .request(
                    generation,
                    json!({"type": "call.join", "data": {"channel_id": "ch"}}),
                    "call.joined",
                    Some(route),
                )
                .await
        });
        let f = peer.recv().await;
        peer.send(json!({ "type": "call.joined", "re": f["id"], "data": { "call_id": "k1", "channel_id": "ch" } })).await;
        peer.send(json!({ "type": "call.subscribe.offer", "data": { "call_id": "k1", "version": 1, "sdp": "v=0" } })).await;
        join.await.unwrap().unwrap();
        // Give the transport time to have handled the offer, then read the mailbox.
        let first = tokio::time::timeout(Duration::from_secs(5), mailbox.recv())
            .await
            .unwrap()
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(5), mailbox.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.ty, "call.joined");
        assert_eq!(second.ty, "call.subscribe.offer");
    }

    #[tokio::test]
    async fn event_for_an_unknown_call_is_dropped() {
        let mut server = TestServer::start().await;
        let (client, mut peer, generation) = connected(&mut server, |_| {}).await;
        let (route, mut mailbox) = mpsc::unbounded_channel::<CallFrame>();
        let c = client.clone();
        let join = tokio::spawn(async move {
            c.commands
                .request(
                    generation,
                    json!({"type": "call.join", "data": {"channel_id": "ch"}}),
                    "call.joined",
                    Some(route),
                )
                .await
        });
        let f = peer.recv().await;
        peer.send(json!({ "type": "call.joined", "re": f["id"], "data": { "call_id": "k1" } }))
            .await;
        join.await.unwrap().unwrap();
        peer.send(json!({ "type": "call.participant", "data": { "call_id": "OTHER", "event": "joined" } })).await;
        peer.send(
            json!({ "type": "call.participant", "data": { "call_id": "k1", "event": "joined" } }),
        )
        .await;
        let _joined = mailbox.recv().await.unwrap();
        let next = tokio::time::timeout(Duration::from_secs(5), mailbox.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.data["call_id"], "k1");
    }

    #[tokio::test]
    async fn abandoned_join_is_left_immediately() {
        let mut server = TestServer::start().await;
        let (client, mut peer, generation) = connected(&mut server, |_| {}).await;
        let (route, _mailbox) = mpsc::unbounded_channel::<CallFrame>();
        let c = client.clone();
        let join = tokio::spawn(async move {
            c.commands
                .request(
                    generation,
                    json!({"type": "call.join", "data": {"channel_id": "ch"}}),
                    "call.joined",
                    Some(route),
                )
                .await
        });
        let f = peer.recv().await;
        join.abort(); // the joiner walks away after `call.join` was written
        let _ = join.await;
        peer.send(json!({ "type": "call.joined", "re": f["id"], "data": { "call_id": "k9" } }))
            .await;
        let leave = peer.recv().await;
        assert_eq!(leave["type"], "call.leave");
        assert_eq!(leave["data"]["call_id"], "k9");
        assert!(client.commands.routes.lock().unwrap().get("k9").is_none());
    }

    #[tokio::test]
    async fn oversized_frame_is_refused_locally() {
        let mut server = TestServer::start().await;
        let (client, mut peer, generation) = connected(&mut server, |_| {}).await;
        let big = json!({ "type": "call.publish", "data": { "sdp": "x".repeat(70 * 1024) } });
        assert_eq!(
            client
                .commands
                .request(generation, big, "call.publish.answer", None)
                .await,
            Err(CommandError::TooLarge)
        );
        let c = client.clone();
        tokio::spawn(async move {
            c.commands
                .request(generation, cmd("call.leave"), "call.ok", None)
                .await
        });
        assert_eq!(peer.recv().await["type"], "call.leave");
    }

    #[tokio::test]
    async fn channel_call_is_broadcast() {
        let mut server = TestServer::start().await;
        let (client, mut peer, _generation) = connected(&mut server, |_| {}).await;
        let mut events = client.events();
        peer.send(json!({ "type": "channel.call", "data": { "channel_id": "ch", "call_id": "k1", "participant_count": 2 } })).await;
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            event,
            ServerEvent::ChannelCall {
                channel_id: "ch".into(),
                call_id: Some("k1".into()),
                participant_count: 2
            }
        );
    }

    /// `call.ice` is fire-and-forget: written on the current socket, refused on a stale one.
    #[tokio::test]
    async fn notify_writes_on_the_current_socket_only() {
        let mut server = TestServer::start().await;
        let (client, mut peer, generation) = connected(&mut server, |_| {}).await;
        assert_eq!(
            client.commands.notify(generation + 1, cmd("call.ice")),
            Err(CommandError::NotSent)
        );
        client.commands.notify(generation, cmd("call.ice")).unwrap();
        assert_eq!(peer.recv().await["type"], "call.ice");
    }

    // ---- P3: re-auth and 1008 recovery ----

    /// A same-user token rotation re-authenticates the open socket: the same `auth` frame
    /// with the new token and an `id`; `ready` with `re` confirms. No new generation.
    #[tokio::test]
    async fn refresh_reauths_the_same_socket() {
        let mut server = TestServer::start().await;
        let (client, mut peer, generation) = connected(&mut server, |_| {}).await;
        client.refresh_now().await.unwrap();
        let reauth = peer.recv().await;
        assert_eq!(reauth["type"], "auth");
        assert_eq!(reauth["data"]["access_token"], "access-2");
        let id = reauth["id"].clone();
        assert!(id.is_string(), "re-auth must carry an id: {reauth}");
        peer.send(json!({ "type": "ready", "re": id, "data": { "user_id": "id-alice" } }))
            .await;
        // Still the same socket and generation: a later command arrives here.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(client.commands.conn().borrow().generation, generation);
        client.commands.notify(generation, cmd("call.ice")).unwrap();
        assert_eq!(peer.recv().await["type"], "call.ice");
    }

    /// A rotation that happens before the socket is ready is applied right after `ready`.
    #[tokio::test]
    async fn rotation_before_ready_reauths_after_ready() {
        let mut server = TestServer::start().await;
        let client = server.client();
        client.login("alice", "pw").await.unwrap();
        client.start_realtime().await.unwrap();
        let mut peer = server.accept().await;
        let auth = peer.recv().await;
        assert_eq!(auth["data"]["access_token"], "access-1");
        client.refresh_now().await.unwrap(); // rotated while not yet ready
        peer.send(json!({ "type": "ready", "data": { "user_id": "id-alice" } }))
            .await;
        let reauth = peer.recv().await;
        assert_eq!(reauth["type"], "auth");
        assert_eq!(reauth["data"]["access_token"], "access-2");
    }

    /// 1008 (e.g. `token_expired`): refresh over REST first, then reconnect with the new
    /// token — never reconnect with the rejected one.
    #[tokio::test]
    async fn auth_close_refreshes_before_reconnecting() {
        let mut server = TestServer::start().await;
        let (_client, mut peer, _generation) = connected(&mut server, |_| {}).await;
        assert_eq!(server.refresh_calls(), 0);
        peer.close(1008, "token_expired").await;
        let mut next = server.accept().await;
        assert_eq!(server.refresh_calls(), 1, "reconnected before refreshing");
        assert_eq!(next.accept_auth().await["data"]["access_token"], "access-2");
    }

    /// A rate-limited refresh is not a rejected token: the session and the user stay.
    #[tokio::test]
    async fn rate_limited_refresh_keeps_the_session() {
        let server = TestServer::start().await;
        let client = server.client();
        client.login("alice", "pw").await.unwrap();
        let before = client.session.snapshot().await.1.unwrap().refresh_token;
        server.set_refresh_mode(RefreshMode::RateLimited(60));
        let outcome = client.refresh_now().await.unwrap();
        assert_eq!(
            outcome,
            RefreshOutcome::RateLimited(Duration::from_secs(60))
        );
        assert!(
            matches!(*client.state().borrow(), AuthState::LoggedIn(_)),
            "signed out"
        );
        assert_eq!(
            client.session.snapshot().await.1.unwrap().refresh_token,
            before
        );
    }

    /// 1008 `rate_limited`: the IP must wait. No refresh (it would not help and only adds
    /// load), no reconnect before 5 s, and after a healthy session the wait starts over.
    #[tokio::test]
    async fn rate_limited_close_backs_off_without_refreshing() {
        let mut server = TestServer::start().await;
        let (client, mut peer, _generation) = connected(&mut server, |_| {}).await;
        peer.close(1008, "rate_limited").await;
        // The ordinary backoff would reconnect after 1 s; rate_limited waits at least 5 s.
        let early = tokio::time::timeout(Duration::from_millis(4500), server.accept()).await;
        assert!(
            early.is_err(),
            "reconnected sooner than the rate-limit minimum"
        );
        let mut next = tokio::time::timeout(Duration::from_secs(10), server.accept())
            .await
            .expect("never reconnected");
        next.accept_auth().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        // After a healthy session, a new rate_limited close waits 5 s again, not 10.
        next.close(1008, "rate_limited").await;
        let early = tokio::time::timeout(Duration::from_millis(4500), server.accept()).await;
        assert!(
            early.is_err(),
            "reconnected sooner than the rate-limit minimum (2nd)"
        );
        tokio::time::timeout(Duration::from_millis(2500), server.accept())
            .await
            .expect("the wait did not start over after a healthy session");
        assert_eq!(
            server.refresh_calls(),
            0,
            "refreshed on a rate_limited close"
        );
        assert!(matches!(*client.state().borrow(), AuthState::LoggedIn(_)));
    }

    /// An auth close whose refresh is rate limited: wait `Retry-After` (not a hot loop), then
    /// reconnect, having asked for a refresh exactly once, and still signed in.
    #[tokio::test]
    async fn auth_close_with_a_rate_limited_refresh_waits() {
        let mut server = TestServer::start().await;
        let (client, mut peer, _generation) = connected(&mut server, |_| {}).await;
        server.set_refresh_mode(RefreshMode::RateLimited(2));
        peer.close(1008, "token_expired").await;
        let early = tokio::time::timeout(Duration::from_millis(1800), server.accept()).await;
        assert!(early.is_err(), "reconnected before Retry-After");
        tokio::time::timeout(Duration::from_secs(6), server.accept())
            .await
            .expect("never reconnected after Retry-After");
        assert_eq!(server.refresh_calls(), 1, "refresh hammered");
        assert!(
            matches!(*client.state().borrow(), AuthState::LoggedIn(_)),
            "signed out"
        );
    }

    /// After a 429, another refresh (any caller) does not ask again inside the wait.
    #[tokio::test]
    async fn a_429_cooldown_is_shared_by_every_refresher() {
        let server = TestServer::start().await;
        let client = server.client();
        client.login("alice", "pw").await.unwrap();
        server.set_refresh_mode(RefreshMode::RateLimited(60));
        client.refresh_now().await.unwrap();
        let second = client.refresh_now().await.unwrap();
        assert!(
            matches!(second, RefreshOutcome::RateLimited(w) if w > Duration::from_secs(50)),
            "{second:?}"
        );
        assert_eq!(server.refresh_calls(), 1, "asked again inside the wait");
    }

    /// A new sign-in does not inherit the previous session's 429 wait.
    #[tokio::test]
    async fn a_new_login_clears_the_429_wait() {
        let server = TestServer::start().await;
        let client = server.client();
        client.login("alice", "pw").await.unwrap();
        server.set_refresh_mode(RefreshMode::RateLimited(60));
        client.refresh_now().await.unwrap();
        client.login("bob", "pw").await.unwrap();
        server.set_refresh_mode(RefreshMode::Rotate);
        assert_eq!(
            client.refresh_now().await.unwrap(),
            RefreshOutcome::Committed
        );
    }

    /// A sign-in during a long `Retry-After` wait on the socket ends the wait: the new session
    /// gets its realtime connection at once.
    #[tokio::test]
    async fn a_new_login_ends_the_sockets_retry_after_wait() {
        let mut server = TestServer::start().await;
        let (client, mut peer, _generation) = connected(&mut server, |_| {}).await;
        server.set_refresh_mode(RefreshMode::RateLimited(3600));
        peer.close(1008, "token_expired").await;
        tokio::time::sleep(Duration::from_millis(300)).await; // the socket task is waiting
        server.set_refresh_mode(RefreshMode::Rotate);
        client.login("bob", "pw").await.unwrap();
        let mut next = tokio::time::timeout(Duration::from_secs(3), server.accept())
            .await
            .expect("the new session waited out the old one's Retry-After");
        next.accept_auth().await;
    }

    /// Auth rejected on a socket that never became ready, refresh fine: back off, not a loop.
    #[tokio::test]
    async fn repeated_auth_rejection_before_ready_backs_off() {
        let mut server = TestServer::start().await;
        let client = server.client();
        client.login("alice", "pw").await.unwrap();
        client.start_realtime().await.unwrap();
        let mut peer = server.accept().await;
        let _auth = peer.recv().await; // never answered with ready
        peer.close(1008, "auth_failed").await;
        let early = tokio::time::timeout(Duration::from_millis(700), server.accept()).await;
        assert!(
            early.is_err(),
            "reconnected at once after a rejection before ready"
        );
    }

    #[test]
    fn retry_after_is_clamped() {
        use crate::client::{parse_retry_after, REFRESH_RETRY_INTERVAL};
        assert_eq!(parse_retry_after(Some("0")), Duration::from_secs(1));
        assert_eq!(parse_retry_after(Some(" 30 ")), Duration::from_secs(30));
        assert_eq!(parse_retry_after(Some("999999")), Duration::from_secs(3600));
        assert_eq!(
            parse_retry_after(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            REFRESH_RETRY_INTERVAL
        );
        assert_eq!(parse_retry_after(None), REFRESH_RETRY_INTERVAL);
    }

    #[test]
    fn refresh_loop_delay_honours_retry_after() {
        use crate::client::{next_refresh_delay, REFRESH_RETRY_INTERVAL};
        let long = Duration::from_secs(120);
        assert_eq!(
            next_refresh_delay(&Ok(RefreshOutcome::RateLimited(long))),
            long
        );
        // Never sooner than the usual retry.
        let short = Duration::from_secs(1);
        assert_eq!(
            next_refresh_delay(&Ok(RefreshOutcome::RateLimited(short))),
            REFRESH_RETRY_INTERVAL
        );
    }

    /// A refused re-auth (different user / invalid) closes the socket and reconnects.
    #[tokio::test]
    async fn refused_reauth_reconnects() {
        let mut server = TestServer::start().await;
        let (client, mut peer, _generation) = connected(&mut server, |_| {}).await;
        client.refresh_now().await.unwrap();
        let reauth = peer.recv().await;
        peer.send(json!({ "type": "error", "re": reauth["id"], "data": { "code": "auth_failed", "message": "no" } })).await;
        peer.expect_closed().await;
        let mut next = server.accept().await;
        next.accept_auth().await;
    }

    /// 1008 and the refresh token is rejected too: signed out, and no reconnect loop.
    #[tokio::test]
    async fn auth_close_with_rejected_refresh_signs_out() {
        let mut server = TestServer::start().await;
        let (client, mut peer, _generation) = connected(&mut server, |_| {}).await;
        server.set_refresh_mode(RefreshMode::Fail(401));
        peer.close(1008, "token_expired").await;
        let mut state = client.state();
        tokio::time::timeout(
            Duration::from_secs(5),
            state.wait_for(|s| *s == AuthState::LoggedOut),
        )
        .await
        .expect("never signed out")
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1500), server.sockets.recv())
                .await
                .is_err(),
            "reconnected without a session"
        );
    }
}
