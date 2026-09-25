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
pub enum RefreshMode {
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
    pub fn client(&self) -> BrookClient {
        BrookClient::new(CoreConfig::new(&self.base).unwrap()).unwrap()
    }

    pub fn set_refresh_mode(&self, mode: RefreshMode) {
        self.state.lock().unwrap().refresh_mode = mode;
    }

    pub fn refresh_calls(&self) -> u32 {
        self.state.lock().unwrap().refresh_calls
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
