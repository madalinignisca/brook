//! The offline cache and outbox on [`BrookClient`] (plan C5; spec §8): turned on with
//! [`BrookClient::enable_local_data`], then following whoever signs in.
//!
//! Everything here answers `local.unavailable` (an [`Error::Api`] code) while no local data
//! is open: stores switched off, a locked or damaged key store, or nobody signed in. The app
//! then works online-only, exactly as before.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::cache::CacheEvent;
use crate::cache_http::Http;
use crate::local::LocalData;
use crate::offline::{Net, Offline};
use crate::outbox::{Deleted, OutboxError, PendingMessage};
use crate::{BrookClient, Channel, Error, KeySlot, Message, Result};

/// A page of cached messages, newest first.
#[derive(Debug, Clone)]
pub struct CachedMessages {
    pub messages: Vec<Message>,
    /// The cache can't prove this page complete: show it with a loading marker and call
    /// [`BrookClient::load_head`] (no `before`) or [`BrookClient::load_older`].
    pub needs_network: bool,
}

fn unavailable() -> Error {
    Error::Api {
        code: "local.unavailable".into(),
        message: "offline storage isn't available".into(),
    }
}

fn store_error() -> Error {
    Error::Api {
        code: "local.store".into(),
        message: "offline storage failed".into(),
    }
}

fn outbox_error(e: OutboxError) -> Error {
    match e {
        OutboxError::NotSent { reason, .. } => Error::Api {
            code: format!("outbox.not_sent.{reason}"),
            message: "the message wasn't sent".into(),
        },
        other => Error::Api {
            code: match other {
                OutboxError::WouldOvertake => "outbox.would_overtake",
                OutboxError::SignedOut => "outbox.signed_out",
                OutboxError::Closed => "outbox.closed",
                OutboxError::IdInUse => "outbox.id_in_use",
                _ => "outbox.store",
            }
            .into(),
            message: other.to_string(),
        },
    }
}

impl BrookClient {
    fn net(&self) -> Net {
        let http = Arc::new(Http {
            http: self.http.clone(),
            base: self.base.clone(),
            session: self.session.clone(),
        });
        Net {
            fetch: http.clone(),
            history: http.clone(),
            post: http,
        }
    }

    fn origin(&self) -> String {
        self.base.as_str().trim_end_matches('/').to_string()
    }

    /// Turn on the offline cache and outbox, keyed in `slot` (the platform's secure store,
    /// the same one `enable_persistence` uses), stored under `data_dir`. From then on the
    /// signed-in user's stores open on sign-in. Returns whether local data is on: false while
    /// stores are switched off in this build, or if the key store is locked or damaged
    /// (nothing is deleted; the app works online-only).
    pub async fn enable_local_data(&self, slot: Arc<dyn KeySlot>, data_dir: PathBuf) -> bool {
        if !crate::store::stores_enabled() {
            return false;
        }
        let Ok(Some(local)) = LocalData::open(&data_dir.join("stores"), slot).await else {
            return false;
        };
        if local.reconcile().await.is_err() {
            return false;
        }
        if local.take_lost_unsent() {
            let _ = self.cache_events.send(CacheEvent::OutboxLost);
        }
        *self.offline.lock().await = Some(Offline::with_events(local, self.cache_events.clone()));
        self.session.note_runtime();
        let (offline, net, origin) = (self.offline.clone(), self.net(), self.origin());
        let session = self.session.clone();
        let mut revisions = session.watch();
        let raw = self.commands.raw_events_sender();
        let mut shutdown = self.shutdown_signal();
        let task = tokio::spawn(async move {
            loop {
                revisions.borrow_and_update();
                {
                    // The session is read under the lock, not taken from the change that woke
                    // this: a sign-out or switch since then is what counts. User and epoch
                    // come from one snapshot, so the outbox never sends under another user.
                    let mut guard = offline.lock().await;
                    if let Some(off) = guard.as_mut() {
                        match session.snapshot().await {
                            (rev, Some(s)) => {
                                let _ = off
                                    .signed_in(
                                        &origin,
                                        &s.user.id,
                                        rev.epoch,
                                        net.clone(),
                                        raw.subscribe(),
                                    )
                                    .await;
                            }
                            (_, None) => off.signed_out(),
                        }
                    }
                }
                let stop = tokio::select! {
                    changed = revisions.changed() => changed.is_err(),
                    _ = shutdown.changed() => true, // the client is gone
                };
                if stop {
                    // Close the stores (their threads joined) rather than leave them to
                    // whenever the last handle drops.
                    // Held through the close: an empty slot means everything is shut.
                    let mut guard = offline.lock().await;
                    if let Some(off) = guard.take() {
                        off.close().await;
                    }
                    return;
                }
            }
        });
        self.track(task);
        true
    }

    /// Change notices from the cache and outbox, whoever is signed in. Re-read what's shown.
    /// A receiver that lags (`RecvError::Lagged`) re-reads everything.
    pub fn cache_events(&self) -> broadcast::Receiver<CacheEvent> {
        self.cache_events.subscribe()
    }

    /// Sign out, first erasing this user's local data ("Remove this device's data", #46
    /// §8). The erase is local and happens first, whether or not the server can be reached.
    pub async fn sign_out_and_forget(&self) -> Result<()> {
        let erased = match (self.offline.lock().await.as_mut(), self.who().await) {
            (Some(off), Some((user, epoch))) => off
                .forget(&self.origin(), &user, epoch)
                .await
                .map_err(|_| store_error()),
            _ => Ok(()),
        };
        self.logout().await;
        erased
    }

    /// Syncing, last synced, and whether the last sync couldn't reach the server (spec
    /// §8's `cache_state`). `local.unavailable` while no local data is open.
    pub async fn cache_state(&self) -> Result<crate::cache::CacheState> {
        Ok(self.active_cache().await?.state().borrow().clone())
    }

    /// Unsent messages (the sign-out warning: "2 messages haven't been sent").
    pub async fn unsent_count(&self) -> u64 {
        match (self.offline.lock().await.as_ref(), self.who().await) {
            (Some(off), Some((user, _))) => off.unsent(&self.origin(), &user).await,
            _ => 0,
        }
    }

    /// Other users with data on this device (#46 §8): after a different user signs in, the
    /// app says so and then calls [`BrookClient::wipe_other_local_users`].
    pub async fn other_local_users(&self) -> Result<Vec<(String, String)>> {
        let guard = self.offline.lock().await;
        let off = guard.as_ref().ok_or_else(unavailable)?;
        let (user, _) = self.who().await.ok_or_else(unavailable)?;
        off.others(&self.origin(), &user)
            .await
            .map_err(|_| store_error())
    }

    /// Erase every other user's data on this device (everyone but whoever is signed in
    /// now, whether or not their own stores have opened yet).
    pub async fn wipe_other_local_users(&self) -> Result<()> {
        let mut guard = self.offline.lock().await;
        let off = guard.as_mut().ok_or_else(unavailable)?;
        let (user, _) = self.who().await.ok_or_else(unavailable)?;
        off.wipe_others(&self.origin(), &user)
            .await
            .map_err(|_| store_error())
    }

    /// The cached channels the signed-in user is in, with unread counts computed locally.
    pub async fn cached_channels(&self) -> Result<Vec<Channel>> {
        let cache = self.active_cache().await?;
        let rows = cache.cached_channels().await.map_err(|_| store_error())?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let mut ch: Channel = serde_json::from_value(row.json).ok()?;
                ch.unread_count = i64::from(row.unread);
                Some(ch)
            })
            .collect())
    }

    /// Cached messages, **newest first** (unlike `channel_history`, which is oldest first).
    /// Never touches the network.
    pub async fn cached_messages(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<CachedMessages> {
        let cache = self.active_cache().await?;
        let page = cache
            .cached_messages(channel_id, before, limit)
            .await
            .map_err(|_| store_error())?;
        Ok(CachedMessages {
            messages: page
                .messages
                .into_iter()
                .filter_map(|m| serde_json::from_value(m).ok())
                .collect(),
            needs_network: page.needs_network,
        })
    }

    /// Fetch the newest page of a channel into the cache (after `needs_network`).
    pub async fn load_head(&self, channel_id: &str, limit: usize) -> Result<()> {
        self.active_cache()
            .await?
            .load_head(channel_id, limit)
            .await
    }

    /// Fetch the page below what's cached into the cache.
    pub async fn load_older(&self, channel_id: &str, limit: usize) -> Result<()> {
        self.active_cache()
            .await?
            .load_older(channel_id, limit)
            .await
    }

    /// Queue a message: saved before this returns, sent in order when possible. Pass the
    /// `client_id` of an earlier attempt to retry it. Returns its `client_id`.
    pub async fn send_queued(
        &self,
        channel_id: &str,
        body: &str,
        client_id: Option<String>,
    ) -> Result<String> {
        let outbox = self.active_outbox().await?;
        outbox
            .enqueue(channel_id, body, client_id)
            .await
            .map_err(outbox_error)
    }

    /// A channel's messages that haven't gone out, in the order they will.
    pub async fn pending_messages(&self, channel_id: &str) -> Result<Vec<PendingMessage>> {
        let outbox = self.active_outbox().await?;
        outbox.pending(channel_id).await.map_err(|_| store_error())
    }

    /// Put a failed message back in line.
    pub async fn retry_send(&self, client_id: &str) -> Result<()> {
        self.active_outbox()
            .await?
            .retry(client_id)
            .await
            .map_err(outbox_error)
    }

    /// Remove a message that hasn't gone out (`AlreadySent` if the server has it).
    pub async fn delete_pending(&self, client_id: &str) -> Result<Deleted> {
        self.active_outbox()
            .await?
            .delete_pending(client_id)
            .await
            .map_err(outbox_error)
    }

    /// Who the session is signed in as, and its epoch, from one snapshot.
    async fn who(&self) -> Option<(String, u64)> {
        match self.session.snapshot().await {
            (rev, Some(s)) => Some((s.user.id, rev.epoch)),
            (_, None) => None,
        }
    }

    /// The open stores, only if they are the signed-in user's: between a switch and the
    /// watcher catching up, the previous user's stores are still open and must not answer.
    async fn active(&self) -> Result<(Arc<crate::cache::Cache>, Arc<crate::outbox::Outbox>)> {
        let guard = self.offline.lock().await;
        let (user, _) = self.who().await.ok_or_else(unavailable)?;
        guard
            .as_ref()
            .and_then(|off| off.active_for(&self.origin(), &user))
            .map(|a| (a.cache.clone(), a.outbox.clone()))
            .ok_or_else(unavailable)
    }

    async fn active_cache(&self) -> Result<Arc<crate::cache::Cache>> {
        Ok(self.active().await?.0)
    }

    async fn active_outbox(&self) -> Result<Arc<crate::outbox::Outbox>> {
        Ok(self.active().await?.1)
    }
}
