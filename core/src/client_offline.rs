//! The offline cache and outbox on [`BrookClient`] (plan C5; spec §8): turned on with
//! [`BrookClient::enable_local_data`], then following whoever signs in.
//!
//! Everything here answers `local.unavailable` (an [`Error::Api`] code) while no local data
//! is open: stores switched off, a locked or damaged key store, or nobody signed in. The app
//! then works online-only, exactly as before.

use std::path::{Path, PathBuf};
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
                OutboxError::BadId => "outbox.bad_id",
                OutboxError::TooManyFiles => "outbox.too_many_files",
                OutboxError::FileTooLarge => "outbox.file_too_large",
                OutboxError::EmptyFile => "outbox.empty_file",
                OutboxError::EmptyMessage => "outbox.empty_message",
                OutboxError::FileUnreadable => "outbox.file_unreadable",
                OutboxError::Cancelled => "transfer.cancelled",
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
            transfers: self.transfers.clone(),
        });
        Net {
            fetch: http.clone(),
            history: http.clone(),
            post: http.clone(),
            upload: http.clone(),
            download: http,
            transfers: self.transfers.clone(),
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
        // A loss is recorded even if reconciling then fails part-way: the outbox that held
        // it may already be gone.
        let reconciled = local.reconcile().await;
        if local.take_lost_unsent() {
            crate::offline::record_loss(&self.losses, &self.cache_events);
        }
        if reconciled.is_err() {
            return false;
        }
        *self.offline.lock().await = Some(Offline::with_events(
            local,
            self.cache_events.clone(),
            crate::offline::StateFeed::new(self.cache_state.clone()),
            self.losses.clone(),
        ));
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

    /// The signed-in user's sync state, following sign-ins and switches: the default
    /// while nobody's stores are open or nobody is signed in.
    pub fn subscribe_cache_state(&self) -> tokio::sync::watch::Receiver<crate::cache::CacheState> {
        self.cache_state.subscribe()
    }

    /// The newest loss of unsent messages the app hasn't acknowledged (say "some messages
    /// couldn't be kept"), or `None`. Read at start and after `OutboxLost` or `Reset`.
    pub fn outbox_lost(&self) -> Option<u64> {
        self.losses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current()
    }

    /// The app told the user about loss `n`. A newer loss stays reported.
    pub fn acknowledge_outbox_lost(&self, n: u64) {
        self.losses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .acknowledge(n);
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

    /// Cached profiles by id (the ids the cache doesn't know are left out): after a `Users`
    /// notice, the app redraws those authors with their current names.
    pub async fn cached_users(&self, ids: &[String]) -> Result<Vec<crate::chat::ChannelMember>> {
        let cache = self.active_cache().await?;
        cache
            .cached_users(ids.to_vec())
            .await
            .map_err(|_| store_error())
    }

    /// The cached channels the signed-in user is in, with unread counts computed locally.
    pub async fn cached_channels(&self) -> Result<Vec<Channel>> {
        let cache = self.active_cache().await?;
        let rows = cache.cached_channels().await.map_err(|_| store_error())?;
        let total = rows.len();
        let channels: Vec<Channel> = rows
            .into_iter()
            .filter_map(|row| {
                let mut ch: Channel = serde_json::from_value(row.json).ok()?;
                ch.unread_count = i64::from(row.unread);
                Some(ch)
            })
            .collect();
        if channels.len() < total {
            tracing::debug!(
                dropped = total - channels.len(),
                "unreadable cached channels"
            );
        }
        Ok(channels)
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
        let total = page.messages.len();
        let messages: Vec<Message> = page
            .messages
            .into_iter()
            .filter_map(|m| serde_json::from_value(m).ok())
            .collect();
        if messages.len() < total {
            tracing::debug!(
                dropped = total - messages.len(),
                "unreadable cached messages"
            );
        }
        Ok(CachedMessages {
            messages,
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

    /// Queue a message (a reply, with `reply_to_id`): saved before this returns, sent in
    /// order when possible. The same `client_id` again returns the stored message, reply
    /// target included: quoting something else needs a new id. Pass the
    /// `client_id` of an earlier attempt to retry it (a UUID, any case; anything else is
    /// `outbox.bad_id`). Returns its `client_id` in canonical lowercase form, which is how
    /// `pending_messages` and the sent message's `client_id` will show it.
    pub async fn send_queued(
        &self,
        channel_id: &str,
        body: &str,
        reply_to_id: Option<String>,
        client_id: Option<String>,
    ) -> Result<String> {
        let outbox = self.active_outbox().await?;
        outbox
            .enqueue(channel_id, body, reply_to_id, client_id)
            .await
            .map_err(outbox_error)
    }

    /// Queue a message with files (up to [`crate::MAX_FILES_PER_MESSAGE`], each at most
    /// [`crate::MAX_FILE_BYTES`], none empty; the body may be empty). Each file is copied into
    /// an encrypted snapshot before this returns (progress: `Preparing` on its transfer id),
    /// so the paths are read only now. Call it off the UI thread: copying can take seconds.
    /// The receipt's transfer ids carry upload progress, and `cancel_transfer` on any of
    /// them cancels the message's sending (Retry resumes it). The same `client_id` again
    /// returns the stored message's receipt, copying nothing.
    pub async fn send_queued_with_files(
        &self,
        channel_id: &str,
        body: &str,
        reply_to_id: Option<String>,
        client_id: Option<String>,
        files: Vec<crate::OutgoingFile>,
    ) -> Result<crate::SendReceipt> {
        let outbox = self.active_outbox().await?;
        outbox
            .enqueue_with_files(channel_id, body, reply_to_id, client_id, files)
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

    /// Retry a failed reply without its quote (after `message.reply_target_gone`), keeping
    /// its place in the queue.
    pub async fn retry_without_reply(&self, client_id: &str) -> Result<()> {
        self.active_outbox()
            .await?
            .retry_without_reply(client_id)
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

    async fn active_files(&self) -> Result<Arc<crate::files::Files>> {
        let guard = self.offline.lock().await;
        let (user, _) = self.who().await.ok_or_else(unavailable)?;
        guard
            .as_ref()
            .and_then(|off| off.active_for(&self.origin(), &user))
            .map(|a| a.files.clone())
            .ok_or_else(unavailable)
    }

    /// Download attachment `file_id` into this device's file cache (encrypted), resuming a
    /// partial, or join the download of it already running. Progress and cancel under `id`
    /// ([`BrookClient::transfer_events`], [`BrookClient::cancel_transfer`]). The file is found
    /// in the cached messages: `file.unknown` if none lists it, `file.gone` if the server
    /// deleted it (it's dropped from the cache), `local.unavailable` without local data.
    /// `transfer.paused`: the session ended (signed out, or another user); call again once
    /// signed in, and it resumes.
    pub async fn cache_file(&self, id: crate::TransferId, file_id: &str) -> Result<()> {
        self.active_files().await?.cache_file(id, file_id).await
    }

    /// Cache `file_id` (as [`BrookClient::cache_file`]), then decrypt it into a private
    /// per-user directory and return that path, for the system to open. Refused
    /// (`file.open_refused`) for executables and launchers: those are Save only. The copy is
    /// removed by [`BrookClient::clear_open_copies`], at the next start, and at sign-out.
    pub async fn open_file(&self, id: crate::TransferId, file_id: &str) -> Result<PathBuf> {
        self.active_files().await?.open_file(id, file_id).await
    }

    /// An image attachment's bytes for a **sandboxed** decoder: only PNG, JPEG, GIF or WebP
    /// (sniffed from the bytes), at most `PREVIEW_MAX_BYTES` (checked before anything is
    /// fetched), with the header's size within `PREVIEW_MAX_SIDE` / `PREVIEW_MAX_PIXELS`.
    /// Fetched into the file cache (progress and cancel under `id`), decrypted into memory,
    /// never written to disk. `file.preview_refused` otherwise.
    pub async fn preview_file(
        &self,
        id: crate::TransferId,
        file_id: &str,
    ) -> Result<crate::ImagePreview> {
        self.active_files().await?.preview_file(id, file_id).await
    }

    /// Save `file_id` to `destination` from the file cache, if it's complete there (so it
    /// works offline). `Ok(false)`: not cached; download it as before
    /// ([`BrookClient::download_file`]). A failed save leaves nothing at `destination`.
    pub async fn save_cached_file(&self, file_id: &str, destination: &Path) -> Result<bool> {
        Ok(self
            .active_files()
            .await?
            .save_from_cache(file_id, destination)
            .await?
            .is_some())
    }

    /// Whether `file_id` is in this device's file cache. Re-read on
    /// [`crate::CacheEvent::Files`].
    pub async fn file_state(&self, file_id: &str) -> Result<crate::FileCacheState> {
        self.active_files().await?.state(file_id).await
    }

    /// "Keep available offline": `file_id` is downloaded now, or as soon as there's a
    /// connection, and kept until unpinned (never evicted). Durable across restarts.
    /// `file.unknown` / `file.gone` as for [`BrookClient::cache_file`].
    pub async fn pin_file(&self, file_id: &str) -> Result<()> {
        self.active_files().await?.pin_file(file_id).await
    }

    /// Stop keeping `file_id` offline: it stays cached as an ordinary (evictable) file.
    pub async fn unpin_file(&self, file_id: &str) -> Result<()> {
        self.active_files().await?.unpin_file(file_id).await
    }

    /// How much the pinned files take (they don't count against the cache's cap).
    pub async fn pinned_bytes(&self) -> Result<u64> {
        self.active_files().await?.pinned_bytes().await
    }

    /// Remove the plaintext copies Open made (the app calls this when it quits).
    pub async fn clear_open_copies(&self) {
        if let Ok(files) = self.active_files().await {
            files.clear_open_copies();
        }
    }
}
