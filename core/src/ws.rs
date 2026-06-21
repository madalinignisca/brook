//! WebSocket realtime client.
//!
//! Connects to `/ws`, authenticates with the access token in the first frame,
//! then streams server events onto a broadcast channel the UI subscribes to.
//! Reconnects with capped exponential backoff. Single connection per client.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{broadcast, RwLock};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use url::Url;

use crate::{Channel, Error, Message, Result, Session};

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

/// Run a single connection: auth, then pump events until the socket closes.
/// Returns whether the socket became `ready` (used to decide reconnect backoff).
async fn run_once(url: &Url, token: &str, tx: &broadcast::Sender<ServerEvent>) -> Result<bool> {
    let (mut socket, _resp) = connect_async(url.as_str()).await?;
    tracing::info!(%url, "websocket connected; sending auth");

    let auth = json!({ "type": "auth", "data": { "access_token": token } });
    socket.send(WsMessage::Text(auth.to_string())).await?;

    let mut ready = false;
    while let Some(frame) = socket.next().await {
        match frame? {
            WsMessage::Text(text) => match serde_json::from_str::<Envelope>(text.as_str()) {
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
                        Err(err) => tracing::warn!(%err, "failed to parse reaction.update payload"),
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
            },
            WsMessage::Close(frame) => {
                tracing::debug!(?frame, "websocket closed by server");
                break;
            }
            _ => {} // ping/pong handled by tungstenite; ignore binary
        }
    }
    Ok(ready)
}

/// Reconnect loop: keep a live connection, backing off on failure. Runs for the
/// life of the client (events are dropped when there are no subscribers).
pub(crate) async fn run(
    url: Url,
    session: Arc<RwLock<Option<Session>>>,
    tx: broadcast::Sender<ServerEvent>,
) {
    let mut backoff = 1u64;
    loop {
        // Read the *current* access token each (re)connect, so after a background
        // refresh the socket reconnects authorized rather than with a stale token.
        let token = match session.read().await.as_ref() {
            Some(s) => s.access_token.clone(),
            None => {
                // Not logged in (yet/anymore): idle and re-check instead of exiting,
                // so a later login restarts the socket without a new task.
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        match run_once(&url, &token, &tx).await {
            // Reset backoff only after a usable (ready) session, so an immediate
            // auth-close (e.g. expired token) backs off instead of spin-reconnecting.
            Ok(true) => backoff = 1,
            Ok(false) => tracing::warn!("websocket closed before becoming ready; backing off"),
            Err(err) => tracing::warn!(%err, "websocket connection error; will reconnect"),
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}
