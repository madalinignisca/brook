//! WebSocket realtime client.
//!
//! Connects to `/ws`, authenticates with the access token in the first frame,
//! then streams server events onto a broadcast channel the UI subscribes to.
//! Reconnects with capped exponential backoff. Single connection per client.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use url::Url;

use crate::client::{RefreshOutcome, Refresher};
use crate::session_store::{Revision, SessionStore};
use crate::{Channel, Error, Message, Result};

/// A realtime event pushed from the server.
///
/// `#[non_exhaustive]`: more kinds (typing, presence, calls) will be added, so
/// consumers must include a catch-all arm and won't break when they land.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServerEvent {
    /// The socket authenticated and is now subscribed to fan-out.
    Ready,
    /// A new message arrived in a channel the user belongs to.
    MessageNew(Message),
    /// An existing message was edited (carries the updated message).
    MessageUpdate(Message),
    /// A message was deleted.
    MessageDelete {
        /// The channel the message belonged to.
        channel_id: String,
        /// The deleted message's id.
        message_id: String,
    },
    /// A reaction was added or removed on a message.
    ReactionUpdate {
        /// The channel the message is in.
        channel_id: String,
        /// The reacted-to message.
        message_id: String,
        /// The emoji.
        emoji: String,
        /// The user who toggled it.
        user_id: String,
        /// True if added, false if removed.
        added: bool,
        /// The new total count for this emoji on the message.
        count: i64,
    },
    /// A channel's membership/metadata changed (e.g. the user was added to it).
    ChannelUpdate(Channel),
    /// A channel was deleted.
    ChannelDelete {
        /// The deleted channel's id.
        channel_id: String,
    },
    /// A call started, changed size, or ended in a channel the user belongs to
    /// (`call_id` is `None` once it ended). Sent for every call in progress right after
    /// the socket becomes ready, then on each change.
    ChannelCall {
        /// The channel the call belongs to.
        channel_id: String,
        /// The call, or `None` when it ended.
        call_id: Option<String>,
        /// How many participants are in it now.
        participant_count: u32,
    },
    /// Someone is typing in a channel (ephemeral; expire client-side).
    Typing {
        /// The channel they're typing in.
        channel_id: String,
        /// The typing user's id.
        user_id: String,
        /// The typing user's display name (for "X is typing…").
        display_name: String,
    },
}

/// Minimum wait before reconnecting after a `rate_limited` close (the close carries no
/// `Retry-After`).
const RATE_LIMIT_BACKOFF_SECS: u64 = 5;
/// How long the server has to answer a command, counted from when it was written.
pub(crate) const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// Client frames above this are refused locally (the server closes with 1009).
const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Commands waiting for the socket writer; beyond this, callers get `Busy`.
const COMMAND_QUEUE: usize = 64;

/// The socket as the command layer sees it. `generation` increases for every socket that
/// becomes ready; a command is only ever written on the generation it was created for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Conn {
    pub(crate) generation: u64,
    pub(crate) ready: bool,
}

/// A server frame routed to the call task that owns its `call_id`.
#[derive(Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))] // read by the call layer (C1b P4)
pub(crate) struct CallFrame {
    pub(crate) ty: String,
    pub(crate) data: Value,
}

pub(crate) type CallRoute = mpsc::UnboundedSender<CallFrame>;

/// `call_id` → owning call task. Shared: the transport installs routes (while it processes
/// `call.joined`, before reading the next frame), call tasks remove theirs when they end.
pub(crate) type Routes = Arc<Mutex<HashMap<String, CallRoute>>>;

/// A successful reply.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reply {
    pub(crate) ty: String,
    pub(crate) data: Value,
}

/// Why a command did not get a successful reply. `NotSent` means the server never saw
/// it; `Unknown` means it was written but the socket ended before a reply — the server
/// may or may not have acted on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommandError {
    NotSent,
    Unknown,
    Timeout,
    Rejected { code: String, message: String },
    UnexpectedReply,
    TooLarge,
    Busy,
}

struct Outgoing {
    frame: Value,
    /// The success type expected in the reply (`None`: no reply, e.g. `call.ice`).
    expect: Option<&'static str>,
    generation: u64,
    reply: Option<oneshot::Sender<std::result::Result<Reply, CommandError>>>,
    route: Option<CallRoute>,
}

struct Pending {
    expect: &'static str,
    reply: oneshot::Sender<std::result::Result<Reply, CommandError>>,
    route: Option<CallRoute>,
    deadline: Instant,
}

/// Handle used to send commands over the realtime socket. Cheap to clone.
#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))] // used by the call layer (C1b P4)
pub(crate) struct Commands {
    tx: mpsc::Sender<Outgoing>,
    conn: watch::Receiver<Conn>,
    pub(crate) routes: Routes,
    raw: broadcast::Sender<(String, Value)>,
}

/// The transport side of [`Commands`], consumed by the socket task.
pub(crate) struct Transport {
    rx: mpsc::Receiver<Outgoing>,
    conn: watch::Sender<Conn>,
    routes: Routes,
    pub(crate) reply_timeout: Duration,
    /// Test hook: when set, every command waits for a permit before it is written. The wait
    /// happens off the socket loop (like a slow producer), so the loop keeps servicing frames.
    #[cfg(test)]
    pub(crate) hold_writes: Option<Arc<tokio::sync::Semaphore>>,
    /// Commands released by the test hook come back through here to be written. The sender
    /// is only used by the hook; production builds keep it so the loop's arm never closes.
    #[cfg_attr(not(test), allow(dead_code))]
    released_tx: mpsc::UnboundedSender<Outgoing>,
    released_rx: mpsc::UnboundedReceiver<Outgoing>,
    /// Every server event as it came (`type`, `data`), for the offline cache: its rows carry
    /// the `seq` the typed events drop, and `sync.hint` has no typed event.
    raw: broadcast::Sender<(String, Value)>,
}

pub(crate) fn command_channel() -> (Commands, Transport) {
    let (tx, rx) = mpsc::channel(COMMAND_QUEUE);
    let (conn_tx, conn_rx) = watch::channel(Conn::default());
    let (released_tx, released_rx) = mpsc::unbounded_channel();
    let routes = Routes::default();
    let (raw, _) = broadcast::channel(512);
    (
        Commands {
            tx,
            conn: conn_rx,
            routes: routes.clone(),
            raw: raw.clone(),
        },
        Transport {
            rx,
            conn: conn_tx,
            routes,
            reply_timeout: REPLY_TIMEOUT,
            #[cfg(test)]
            hold_writes: None,
            released_tx,
            released_rx,
            raw,
        },
    )
}

#[cfg_attr(not(test), allow(dead_code))] // the call layer (C1b P4) is the caller
impl Commands {
    pub(crate) fn conn(&self) -> watch::Receiver<Conn> {
        self.conn.clone()
    }

    /// Server events as they came (`type`, `data`), for the offline cache: subscribe per
    /// user's stores.
    pub(crate) fn raw_events_sender(&self) -> broadcast::Sender<(String, Value)> {
        self.raw.clone()
    }

    /// Send `frame` (a `{type, data}` object; the `id` is assigned here) on socket
    /// `generation` and wait for its one reply. Fails fast with `NotSent` if that socket is
    /// not the current, ready one: nothing is ever queued across a disconnect.
    pub(crate) async fn request(
        &self,
        generation: u64,
        frame: Value,
        expect: &'static str,
        route: Option<CallRoute>,
    ) -> std::result::Result<Reply, CommandError> {
        let (reply, rx) = oneshot::channel();
        self.submit(Outgoing {
            frame,
            expect: Some(expect),
            generation,
            reply: Some(reply),
            route,
        })?;
        rx.await.unwrap_or(Err(CommandError::Unknown))
    }

    /// Like [`Commands::request`], but the command is queued **now** (synchronously) and the
    /// reply is awaited separately. The queue is FIFO, so a caller that starts command A and
    /// then notifies B gets A written before B.
    pub(crate) fn start(
        &self,
        generation: u64,
        frame: Value,
        expect: &'static str,
        route: Option<CallRoute>,
    ) -> std::result::Result<
        oneshot::Receiver<std::result::Result<Reply, CommandError>>,
        CommandError,
    > {
        let (reply, rx) = oneshot::channel();
        self.submit(Outgoing {
            frame,
            expect: Some(expect),
            generation,
            reply: Some(reply),
            route,
        })?;
        Ok(rx)
    }

    /// Fire-and-forget (`call.ice`): written on `generation` if it is still current.
    pub(crate) fn notify(
        &self,
        generation: u64,
        frame: Value,
    ) -> std::result::Result<(), CommandError> {
        self.submit(Outgoing {
            frame,
            expect: None,
            generation,
            reply: None,
            route: None,
        })
    }

    fn submit(&self, out: Outgoing) -> std::result::Result<(), CommandError> {
        let conn = *self.conn.borrow();
        if !conn.ready || conn.generation != out.generation {
            return Err(CommandError::NotSent);
        }
        self.tx.try_send(out).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => CommandError::Busy,
            mpsc::error::TrySendError::Closed(_) => CommandError::NotSent,
        })
    }
}

/// Payload of a `channel.delete` envelope.
#[derive(Deserialize)]
struct ChannelDeleted {
    id: String,
}

/// Payload of a `typing` envelope.
#[derive(Deserialize)]
struct TypingEvent {
    channel_id: String,
    user_id: String,
    display_name: String,
}

/// Payload of a `message.delete` envelope.
#[derive(Deserialize)]
struct MessageDeleted {
    id: String,
    channel_id: String,
}

/// Payload of a `reaction.update` envelope.
#[derive(Deserialize)]
struct ReactionChanged {
    channel_id: String,
    message_id: String,
    emoji: String,
    user_id: String,
    added: bool,
    count: i64,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    data: serde_json::Value,
}

/// Derive the `ws(s)://…/ws` URL from the api base URL.
pub(crate) fn ws_url(base: &Url) -> Result<Url> {
    let mut url = base.join("ws")?;
    let scheme = if base.scheme() == "https" {
        "wss"
    } else {
        "ws"
    };
    url.set_scheme(scheme)
        .map_err(|()| Error::UnexpectedResponse)?;
    Ok(url)
}

/// Parse one text frame and publish the event it carries. Returns true for `ready`.
fn dispatch(text: &str, tx: &broadcast::Sender<ServerEvent>) -> bool {
    let mut ready = false;
    match serde_json::from_str::<Envelope>(text) {
        Ok(env) => match env.event_type.as_str() {
            "ready" => {
                ready = true;
                tracing::info!("websocket subscribed (ready)");
                let _ = tx.send(ServerEvent::Ready);
            }
            "message.new" => match serde_json::from_value::<Message>(env.data) {
                Ok(message) => {
                    let _ = tx.send(ServerEvent::MessageNew(message));
                }
                Err(err) => tracing::warn!(%err, "failed to parse message.new payload"),
            },
            "message.update" => match serde_json::from_value::<Message>(env.data) {
                Ok(message) => {
                    let _ = tx.send(ServerEvent::MessageUpdate(message));
                }
                Err(err) => tracing::warn!(%err, "failed to parse message.update payload"),
            },
            "message.delete" => match serde_json::from_value::<MessageDeleted>(env.data) {
                Ok(d) => {
                    let _ = tx.send(ServerEvent::MessageDelete {
                        channel_id: d.channel_id,
                        message_id: d.id,
                    });
                }
                Err(err) => tracing::warn!(%err, "failed to parse message.delete payload"),
            },
            "reaction.update" => match serde_json::from_value::<ReactionChanged>(env.data) {
                Ok(r) => {
                    let _ = tx.send(ServerEvent::ReactionUpdate {
                        channel_id: r.channel_id,
                        message_id: r.message_id,
                        emoji: r.emoji,
                        user_id: r.user_id,
                        added: r.added,
                        count: r.count,
                    });
                }
                Err(err) => {
                    tracing::warn!(%err, "failed to parse reaction.update payload")
                }
            },
            "channel.update" => match serde_json::from_value::<Channel>(env.data) {
                Ok(channel) => {
                    let _ = tx.send(ServerEvent::ChannelUpdate(channel));
                }
                Err(err) => tracing::warn!(%err, "failed to parse channel.update payload"),
            },
            "channel.delete" => match serde_json::from_value::<ChannelDeleted>(env.data) {
                Ok(d) => {
                    let _ = tx.send(ServerEvent::ChannelDelete { channel_id: d.id });
                }
                Err(err) => tracing::warn!(%err, "failed to parse channel.delete payload"),
            },
            "typing" => match serde_json::from_value::<TypingEvent>(env.data) {
                Ok(t) => {
                    let _ = tx.send(ServerEvent::Typing {
                        channel_id: t.channel_id,
                        user_id: t.user_id,
                        display_name: t.display_name,
                    });
                }
                Err(err) => tracing::warn!(%err, "failed to parse typing payload"),
            },
            _ => {} // presence / call — ignored in Phase 1
        },
        Err(err) => tracing::warn!(%err, "failed to parse ws envelope"),
    }
    ready
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// How a connection ended.
#[derive(Debug, PartialEq, Eq)]
enum RunEnd {
    /// Closed for a non-auth reason (or dropped); `ready` says whether it ever authenticated.
    Closed { ready: bool },
    /// The server closed with 1008 (`auth_failed` | `auth_timeout` | `token_expired`): the
    /// credentials must be refreshed over REST before reconnecting.
    AuthRejected { ready: bool },
    /// The server closed with 1008 `rate_limited`: this IP must wait. Refreshing would not help
    /// (and only adds load); back off before reconnecting.
    RateLimited { ready: bool },
    /// The signed-in identity changed; reconnect at once as the new one.
    SessionChanged,
}

/// A re-auth in flight on the open socket.
struct Reauth {
    id: String,
    /// The credential revision whose token it carries.
    rev: u64,
    deadline: Instant,
}

/// Run a single connection: auth, then pump events and commands until the socket closes
/// or the session's identity changes (a login/logout/clear: the socket belongs to the
/// epoch it authenticated in and must not outlive it). A token rotation within the same
/// identity re-authenticates this socket instead (no new generation, no resume).
#[allow(clippy::too_many_arguments)]
async fn run_once(
    url: &Url,
    token: &str,
    rev: Revision,
    session: &SessionStore,
    revisions: &mut watch::Receiver<Revision>,
    tx: &broadcast::Sender<ServerEvent>,
    transport: &mut Transport,
    generation: &mut u64,
) -> Result<RunEnd> {
    let epoch = rev.epoch;
    // The credential revision this socket is authenticated with.
    let mut auth_rev = rev.credential_rev;
    let mut reauth: Option<Reauth> = None;
    let (mut socket, _resp) = connect_async(url.as_str()).await?;
    tracing::info!(%url, "websocket connected; sending auth");

    let auth = json!({ "type": "auth", "data": { "access_token": token } });
    socket.send(WsMessage::Text(auth.to_string())).await?;

    let mut ready = false;
    let mut pending: HashMap<String, Pending> = HashMap::new();
    let mut next_id: u64 = 0;
    let result = loop {
        let next_deadline = pending
            .values()
            .map(|p| p.deadline)
            .chain(reauth.as_ref().map(|r| r.deadline))
            .min();
        tokio::select! {
            changed = revisions.changed() => {
                let now = *revisions.borrow_and_update();
                if changed.is_err() || now.epoch != epoch {
                    tracing::info!("session changed; closing websocket");
                    let _ = socket.close(None).await;
                    break Ok(RunEnd::SessionChanged);
                }
                // Same identity, rotated token: re-auth this socket once it is ready (a
                // rotation before `ready` is picked up right after it).
                if ready && reauth.is_none() && now.credential_rev != auth_rev {
                    match send_reauth(&mut socket, session, *generation, &mut next_id, transport.reply_timeout).await {
                        Ok(r) => reauth = r,
                        Err(err) => break Err(err),
                    }
                }
            }
            Some(out) = transport.rx.recv() => {
                #[cfg(test)]
                if let Some(hold) = transport.hold_writes.clone() {
                    let back = transport.released_tx.clone();
                    tokio::spawn(async move {
                        hold.acquire().await.expect("hold semaphore closed").forget();
                        let _ = back.send(out);
                    });
                    continue;
                }
                if let Err(err) = write_command(&mut socket, transport, out, &mut pending, &mut next_id).await {
                    break Err(err);
                }
            }
            Some(out) = transport.released_rx.recv() => {
                if let Err(err) = write_command(&mut socket, transport, out, &mut pending, &mut next_id).await {
                    break Err(err);
                }
            }
            () = sleep_until_opt(next_deadline) => {
                let now = Instant::now();
                if reauth.as_ref().is_some_and(|r| r.deadline <= now) {
                    tracing::warn!("re-auth not confirmed in time; reconnecting");
                    let _ = socket.close(None).await;
                    break Ok(RunEnd::Closed { ready });
                }
                let expired: Vec<String> = pending
                    .iter()
                    .filter(|(_, p)| p.deadline <= now)
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in expired {
                    if let Some(p) = pending.remove(&id) {
                        let _ = p.reply.send(Err(CommandError::Timeout));
                    }
                }
            }
            frame = socket.next() => {
                let Some(frame) = frame else { break Ok(RunEnd::Closed { ready }) };
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(err) => break Err(err.into()),
                };
                match frame {
                    WsMessage::Text(text) => {
                        // Our re-auth's answer: confirmed (`ready` with `re`) or refused.
                        if let Some(r) = &reauth {
                            if let Ok(v) = serde_json::from_str::<Value>(text.as_str()) {
                                if v.get("re").and_then(Value::as_str) == Some(r.id.as_str()) {
                                    if v["type"] == "ready" {
                                        auth_rev = r.rev;
                                        reauth = None;
                                        tracing::info!("websocket re-authenticated");
                                        // Rotated again meanwhile: re-auth once more.
                                        if revisions.borrow().credential_rev != auth_rev {
                                            match send_reauth(&mut socket, session, *generation, &mut next_id, transport.reply_timeout).await {
                                                Ok(r) => reauth = r,
                                                Err(err) => break Err(err),
                                            }
                                        }
                                        continue;
                                    }
                                    tracing::warn!("re-auth refused; reconnecting");
                                    let _ = socket.close(None).await;
                                    break Ok(RunEnd::Closed { ready });
                                }
                            }
                        }
                        let became_ready = handle_text(text.as_str(), tx, transport, &mut pending, generation, &mut socket, &mut next_id).await;
                        if became_ready {
                            ready = true;
                            tracing::info!("websocket subscribed (ready)");
                            // A rotation that happened before `ready` is not lost.
                            if revisions.borrow().credential_rev != auth_rev {
                                match send_reauth(&mut socket, session, *generation, &mut next_id, transport.reply_timeout).await {
                                    Ok(r) => reauth = r,
                                    Err(err) => break Err(err),
                                }
                            }
                        }
                    }
                    WsMessage::Close(frame) => {
                        let code = frame.as_ref().map(|f| u16::from(f.code));
                        let reason = frame.as_ref().map(|f| f.reason.to_string());
                        tracing::debug!(?code, ?reason, "websocket closed by server");
                        break Ok(match (code, reason.as_deref()) {
                            (Some(1008), Some("rate_limited")) => RunEnd::RateLimited { ready },
                            (Some(1008), _) => RunEnd::AuthRejected { ready },
                            _ => RunEnd::Closed { ready },
                        });
                    }
                    _ => {} // ping/pong handled by tungstenite; ignore binary
                }
            }
        }
    };
    // The socket is gone: nothing more can be written on this generation, and every
    // command still waiting was written but will never get its reply.
    transport.conn.send_modify(|c| c.ready = false);
    for (_, p) in pending.drain() {
        let _ = p.reply.send(Err(CommandError::Unknown));
    }
    result
}

/// Send the auth frame again with the current token (same identity). The reply is a
/// `ready` carrying our `id`. Returns `None` if there is no session any more.
async fn send_reauth(
    socket: &mut Socket,
    session: &SessionStore,
    generation: u64,
    next_id: &mut u64,
    timeout: Duration,
) -> Result<Option<Reauth>> {
    let (rev, current) = session.snapshot().await;
    let Some(token) = current.map(|s| s.access_token) else {
        return Ok(None);
    };
    *next_id += 1;
    let id = format!("{generation}-{next_id}");
    let frame = json!({ "type": "auth", "id": id, "data": { "access_token": token } });
    socket.send(WsMessage::Text(frame.to_string())).await?;
    tracing::debug!("websocket re-auth sent");
    Ok(Some(Reauth {
        id,
        rev: rev.credential_rev,
        deadline: Instant::now() + timeout,
    }))
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Write one command, registering its reply before the write. Only a socket write error
/// is returned (it ends the connection); per-command failures go to the command's caller.
async fn write_command(
    socket: &mut Socket,
    transport: &Transport,
    out: Outgoing,
    pending: &mut HashMap<String, Pending>,
    next_id: &mut u64,
) -> Result<()> {
    let Outgoing {
        mut frame,
        expect,
        generation,
        reply,
        route,
    } = out;
    let fail = |reply: Option<oneshot::Sender<_>>, err| {
        if let Some(reply) = reply {
            let _ = reply.send(Err(err));
        }
    };
    let conn = *transport.conn.borrow();
    if !conn.ready || conn.generation != generation {
        fail(reply, CommandError::NotSent);
        return Ok(());
    }
    if reply.as_ref().is_some_and(|r| r.is_closed()) {
        return Ok(()); // the caller gave up before we wrote it: skip, never send
    }
    *next_id += 1;
    let id = format!("{generation}-{next_id}");
    frame["id"] = Value::String(id.clone());
    let text = frame.to_string();
    if text.len() > MAX_FRAME_BYTES {
        fail(reply, CommandError::TooLarge);
        return Ok(());
    }
    let ty = frame["type"].as_str().unwrap_or("?").to_string();
    if let (Some(expect), Some(reply)) = (expect, reply) {
        pending.insert(
            id.clone(),
            Pending {
                expect,
                reply,
                route,
                // Counted from the write, not from submission.
                deadline: Instant::now() + transport.reply_timeout,
            },
        );
    }
    tracing::debug!(%ty, "websocket command");
    if let Err(err) = socket.send(WsMessage::Text(text)).await {
        if let Some(p) = pending.remove(&id) {
            let _ = p.reply.send(Err(CommandError::Unknown));
        }
        return Err(err.into());
    }
    Ok(())
}

/// Handle one text frame. Returns true when it made the socket `ready`.
#[allow(clippy::too_many_arguments)]
async fn handle_text(
    text: &str,
    tx: &broadcast::Sender<ServerEvent>,
    transport: &Transport,
    pending: &mut HashMap<String, Pending>,
    generation: &mut u64,
    socket: &mut Socket,
    next_id: &mut u64,
) -> bool {
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        tracing::warn!("unparseable websocket frame");
        return false;
    };
    let ty = frame["type"].as_str().unwrap_or_default().to_string();
    let data = frame.get("data").cloned().unwrap_or(Value::Null);

    // Every server event (not a reply to one of ours) also goes out raw, for the cache.
    if frame.get("re").is_none() {
        let _ = transport.raw.send((ty.clone(), data.clone()));
    }

    // A reply to one of our commands.
    if let Some(re) = frame.get("re").and_then(Value::as_str) {
        let Some(p) = pending.remove(re) else {
            tracing::debug!(%ty, "reply for an unknown or expired command; dropped");
            return false;
        };
        let result = if ty == "error" {
            Err(CommandError::Rejected {
                code: data["code"].as_str().unwrap_or_default().to_string(),
                message: data["message"].as_str().unwrap_or_default().to_string(),
            })
        } else if ty == p.expect {
            Ok(Reply {
                ty: ty.clone(),
                data: data.clone(),
            })
        } else {
            tracing::warn!(%ty, expected = p.expect, "unexpected reply type");
            Err(CommandError::UnexpectedReply)
        };
        if ty == "call.joined" && result.is_ok() {
            let call_id = data["call_id"].as_str().unwrap_or_default().to_string();
            if p.reply.is_closed() {
                // The joiner gave up after we sent `call.join`: leave at once so no
                // orphaned participant remains on the server.
                *next_id += 1;
                let leave = json!({
                    "type": "call.leave", "id": format!("{generation}-{next_id}"),
                    "data": { "call_id": call_id },
                });
                let _ = socket.send(WsMessage::Text(leave.to_string())).await;
                return false;
            }
            if let Some(route) = p.route {
                // Install the route and hand it `call.joined` BEFORE reading the next frame,
                // so an offer sent right behind it can never be dropped or seen first.
                let _ = route.send(CallFrame {
                    ty: ty.clone(),
                    data: data.clone(),
                });
                transport.routes.lock().unwrap().insert(call_id, route);
            }
        }
        let _ = p.reply.send(result);
        return false;
    }

    match ty.as_str() {
        "ready" => {
            *generation += 1;
            let generation = *generation;
            transport.conn.send_replace(Conn {
                generation,
                ready: true,
            });
            let _ = tx.send(ServerEvent::Ready);
            true
        }
        "channel.call" => {
            if let Ok(c) = serde_json::from_value::<ChannelCallEvent>(data) {
                let _ = tx.send(ServerEvent::ChannelCall {
                    channel_id: c.channel_id,
                    call_id: c.call_id,
                    participant_count: c.participant_count,
                });
            }
            false
        }
        t if t.starts_with("call.") => {
            let call_id = data["call_id"].as_str().unwrap_or_default().to_string();
            let mut routes = transport.routes.lock().unwrap();
            match routes.get(&call_id) {
                Some(route) => {
                    if route.send(CallFrame { ty, data }).is_err() {
                        routes.remove(&call_id); // its call task is gone
                    }
                }
                None => tracing::debug!(%ty, "call event for an unknown call; dropped"),
            }
            false
        }
        _ => dispatch(text, tx),
    }
}

/// Payload of a `channel.call` envelope.
#[derive(Deserialize)]
struct ChannelCallEvent {
    channel_id: String,
    call_id: Option<String>,
    participant_count: u32,
}

pub(crate) async fn run(
    url: Url,
    session: SessionStore,
    tx: broadcast::Sender<ServerEvent>,
    mut transport: Transport,
    refresher: Refresher,
) {
    let mut revisions = session.watch();
    let mut backoff = 1u64;
    let mut generation = 0u64;
    loop {
        // Read the *current* token and revision on each (re)connect, so after a refresh or
        // a new login the socket authenticates as whoever is signed in now.
        revisions.mark_unchanged();
        let (rev, current) = session.snapshot().await;
        let Some(token) = current.map(|s| s.access_token) else {
            // Not logged in (yet/anymore): wait for a login instead of exiting, so a later
            // login restarts the socket without a new task.
            if revisions.changed().await.is_err() {
                return;
            }
            continue;
        };
        let end = run_once(
            &url,
            &token,
            rev,
            &session,
            &mut revisions,
            &tx,
            &mut transport,
            &mut generation,
        )
        .await;
        let rate_limited = matches!(end, Ok(RunEnd::RateLimited { .. }));
        match end {
            Ok(RunEnd::SessionChanged) => continue, // reconnect at once as the new identity
            Ok(RunEnd::AuthRejected { ready }) => {
                if ready {
                    backoff = 1; // it was a healthy session: earlier failures are history
                }
                // Refresh over REST before reconnecting; reconnecting with the same token
                // would just be rejected again until the grace period is gone.
                match refresher.refresh(rev).await {
                    // A session that was up (its token expired): reconnect at once. One that
                    // never became ready backs off: repeated rejections must not loop.
                    Ok(RefreshOutcome::Committed) | Ok(RefreshOutcome::Discarded) if ready => {
                        backoff = 1;
                        continue;
                    }
                    Ok(RefreshOutcome::Committed) | Ok(RefreshOutcome::Discarded) => {
                        tracing::warn!("auth rejected before ready; backing off")
                    }
                    // Wait as asked, then reconnect (and refresh again if still needed); a
                    // new sign-in or new credentials end the wait at once.
                    Ok(RefreshOutcome::RateLimited(wait)) => {
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => {}
                            _ = revisions.changed() => {}
                        }
                        continue;
                    }
                    // Rejected: the session is cleared (LoggedOut published); the loop idles
                    // until the next login. NoSession: same.
                    Ok(RefreshOutcome::Rejected) | Ok(RefreshOutcome::NoSession) => continue,
                    Err(err) => {
                        tracing::warn!(%err, "refresh after auth close failed; backing off")
                    }
                }
            }
            // Wait before reconnecting, at least RATE_LIMIT_BACKOFF, growing on repeats.
            Ok(RunEnd::RateLimited { ready }) => {
                tracing::warn!("websocket rate limited; backing off");
                if ready {
                    backoff = 1; // a healthy session in between starts the wait over
                }
                backoff = backoff.max(RATE_LIMIT_BACKOFF_SECS);
            }
            // Reset backoff only after a usable (ready) session, so an immediate close
            // backs off instead of spin-reconnecting.
            Ok(RunEnd::Closed { ready: true }) => backoff = 1,
            Ok(RunEnd::Closed { ready: false }) => {
                tracing::warn!("websocket closed before becoming ready; backing off")
            }
            Err(err) => tracing::warn!(%err, "websocket connection error; will reconnect"),
        }
        // A rate-limited IP may wait up to 60 s (the close carries no hint); anything else keeps
        // the ordinary 30 s cap, even right after a rate-limited wait.
        let cap = if rate_limited { 60 } else { 30 };
        backoff = backoff.min(cap);
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(cap);
    }
}
