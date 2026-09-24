//! Test harness: one in-process origin serving the REST auth endpoints and the `/ws`
//! upgrade (the client derives both from a single base URL), with each accepted socket
//! handed to the test to drive imperatively — so tests choose frame order exactly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::{BrookClient, CoreConfig};

/// How `/auth/refresh` answers.
#[derive(Clone, Copy, Debug)]
pub(crate) enum RefreshMode {
    /// Issue a fresh token pair.
    Rotate,
    /// Answer with this status and an error envelope.
    Fail(u16),
}

struct ServerState {
    next: u32,
    /// access token → user handle
    tokens: HashMap<String, String>,
    refresh_mode: RefreshMode,
    refresh_calls: u32,
    sockets: mpsc::UnboundedSender<WsPeer>,
}

type Shared = Arc<Mutex<ServerState>>;

/// A running test origin.
pub(crate) struct TestServer {
    pub(crate) base: String,
    state: Shared,
    sockets: mpsc::UnboundedReceiver<WsPeer>,
}

impl TestServer {
    pub(crate) async fn start() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let state = Arc::new(Mutex::new(ServerState {
            next: 0,
            tokens: HashMap::new(),
            refresh_mode: RefreshMode::Rotate,
            refresh_calls: 0,
            sockets: tx,
        }));
        let app = Router::new()
            .route("/api/v1/auth/login", post(login))
            .route("/api/v1/auth/me", get(me))
            .route("/api/v1/auth/refresh", post(refresh))
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
    pub(crate) fn client(&self) -> BrookClient {
        BrookClient::new(CoreConfig::new(&self.base).unwrap()).unwrap()
    }

    pub(crate) fn set_refresh_mode(&self, mode: RefreshMode) {
        self.state.lock().unwrap().refresh_mode = mode;
    }

    pub(crate) fn refresh_calls(&self) -> u32 {
        self.state.lock().unwrap().refresh_calls
    }

    /// The next socket a client opens (bounded wait).
    pub(crate) async fn accept(&mut self) -> WsPeer {
        tokio::time::timeout(Duration::from_secs(10), self.sockets.recv())
            .await
            .expect("client never connected")
            .expect("server gone")
    }
}

fn issue(state: &mut ServerState, handle: &str) -> Value {
    state.next += 1;
    let access = format!("access-{}", state.next);
    state.tokens.insert(access.clone(), handle.to_string());
    json!({ "access_token": access, "refresh_token": format!("refresh-{}", state.next), "token_type": "bearer" })
}

async fn login(State(state): State<Shared>, Json(body): Json<Value>) -> Response {
    let handle = body["handle"].as_str().unwrap_or_default().to_string();
    Json(issue(&mut state.lock().unwrap(), &handle)).into_response()
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

async fn refresh(State(state): State<Shared>) -> Response {
    let mut state = state.lock().unwrap();
    state.refresh_calls += 1;
    match state.refresh_mode {
        RefreshMode::Rotate => Json(issue(&mut state, "alice")).into_response(),
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
pub(crate) struct WsPeer {
    socket: WebSocket,
}

impl WsPeer {
    /// Next JSON frame from the client (bounded wait; panics on close or timeout).
    pub(crate) async fn recv(&mut self) -> Value {
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

    pub(crate) async fn send(&mut self, frame: Value) {
        self.socket
            .send(AxMessage::Text(frame.to_string().into()))
            .await
            .unwrap();
    }

    /// Standard handshake: expect the `auth` frame, answer `ready` (echoing `re` when present).
    pub(crate) async fn accept_auth(&mut self) -> Value {
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
    pub(crate) async fn expect_closed(&mut self) {
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

    pub(crate) async fn close(mut self, code: u16, reason: &str) {
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
}
