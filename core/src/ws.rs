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

use crate::{Error, Message, Result, Session};

/// A realtime event pushed from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEvent {
    /// The socket authenticated and is now subscribed to fan-out.
    Ready,
    /// A new message arrived in a channel the user belongs to.
    MessageNew(Message),
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
                    _ => {} // typing / presence / call — ignored in Phase 1
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
