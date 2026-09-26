//! Offline data (the encrypted cache and outbox, plan C5) across the FFI: records, the
//! client methods, and two listeners (spec 2026-09-25-offline-ffi-spec.md).
//!
//! Errors are `LoginError::Api { code }` with core's codes: `local.unavailable` (no local
//! data open for the signed-in user; the app works online-only), `local.store`, and
//! `outbox.*`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use brook_core::{
    CacheEvent, CacheState, Deleted, PendingState, TransferEvent, TransferId, TransferState,
};
use tokio::sync::broadcast::{self, error::RecvError};

use crate::call::run;
use crate::client::FfiBrookClient;
use crate::listener::{subscribe_watch, Subscription};
use crate::runtime::runtime;
use crate::types::LoginError;

/// A channel member, as the UI names them (DM titles).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiMember {
    pub id: String,
    pub handle: String,
    pub display_name: String,
}

/// A cached channel, with its unread count computed on this device.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiCachedChannel {
    pub id: String,
    /// `"channel"` or `"dm"`.
    pub kind: String,
    pub name: Option<String>,
    pub archived: bool,
    pub unread_count: i64,
    pub members: Vec<FfiMember>,
}

impl From<brook_core::Channel> for FfiCachedChannel {
    fn from(c: brook_core::Channel) -> Self {
        Self {
            id: c.id,
            kind: c.kind,
            name: c.name,
            archived: c.archived,
            unread_count: c.unread_count,
            members: c
                .members
                .into_iter()
                .map(|m| FfiMember {
                    id: m.id,
                    handle: m.handle,
                    display_name: m.display_name,
                })
                .collect(),
        }
    }
}

/// An attached file, as the server describes it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiFileInfo {
    pub id: String,
    /// The sanitised ASCII name to **save** under (safe on every OS).
    pub filename: String,
    /// The name as typed: display text only, never a filesystem name.
    pub original_name: String,
    pub size: u64,
    /// Declared, untrusted: only for choosing an icon.
    pub content_type: String,
    /// Hex sha256, once committed (a download is checked against it).
    pub sha256: Option<String>,
}

impl From<brook_core::FileInfo> for FfiFileInfo {
    fn from(f: brook_core::FileInfo) -> Self {
        Self {
            id: f.id,
            filename: f.filename,
            original_name: f.original_name,
            size: f.size,
            content_type: f.content_type,
            sha256: f.sha256,
        }
    }
}

/// The quoted message of a reply. Label it from `deleted` and `attachments`, not the text.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiReplyExcerpt {
    pub id: String,
    pub author_display_name: Option<String>,
    pub body: String,
    pub deleted: bool,
    pub attachments: u32,
}

/// A message (cached or from the network). A deleted one keeps its place with `deleted` set
/// and an empty body.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiMessage {
    pub id: String,
    pub channel_id: String,
    pub author_id: String,
    pub author_handle: Option<String>,
    pub author_display_name: Option<String>,
    pub body: String,
    /// ISO-8601.
    pub created_at: String,
    /// The sender's outbox id: drop the pending row with this id once the message is here.
    pub client_id: Option<String>,
    pub deleted: bool,
    /// ISO-8601, if it was edited.
    pub edited_at: Option<String>,
    pub reply_to_id: Option<String>,
    pub reply_to: Option<FfiReplyExcerpt>,
    /// In the order the sender gave them (none on a tombstone).
    pub attachments: Vec<FfiFileInfo>,
}

impl From<brook_core::Message> for FfiMessage {
    fn from(m: brook_core::Message) -> Self {
        let deleted = m.is_deleted();
        Self {
            id: m.id,
            channel_id: m.channel_id,
            author_id: m.author_id,
            author_handle: m.author_handle,
            author_display_name: m.author_display_name,
            body: if deleted { String::new() } else { m.body },
            created_at: m.created_at,
            client_id: m.client_id,
            deleted,
            edited_at: m.edited_at,
            reply_to_id: m.reply_to_id,
            reply_to: m.reply_to.map(|r| FfiReplyExcerpt {
                id: r.id,
                author_display_name: r.author_display_name,
                body: r.body,
                deleted: r.deleted,
                attachments: r.attachments,
            }),
            attachments: if deleted {
                vec![]
            } else {
                m.attachments.into_iter().map(Into::into).collect()
            },
        }
    }
}

/// A page of cached messages, **newest first**.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiCachedMessages {
    pub messages: Vec<FfiMessage>,
    /// Not provably complete: show it with a loading marker and call `load_head` or
    /// `load_older`; a `Channels` event follows when the cache has more.
    pub needs_network: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiPendingState {
    Pending,
    Sending,
    /// The server has it; the cache hasn't caught up yet (show it as sending).
    Accepted,
    /// Refused: `code` is the server's (e.g. `not_found`, `authz.forbidden`). Retry or delete.
    Failed {
        code: String,
    },
}

/// A queued message's file. `transfer_id` carries its progress (`subscribe_transfers`) and
/// cancels the message's sending (`cancel_transfer`); it's per process.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiPendingFile {
    pub file_client_id: String,
    pub transfer_id: u64,
    pub filename: String,
    pub size: u64,
    pub uploaded: bool,
    /// The server's refusal of this file, if it's the one that failed the message.
    pub error: Option<String>,
}

/// A message that hasn't gone out, in send order.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiPendingMessage {
    pub client_id: String,
    pub channel_id: String,
    pub body: String,
    /// The quoted message, for a reply ("Replying to …").
    pub reply_to_id: Option<String>,
    pub files: Vec<FfiPendingFile>,
    pub state: FfiPendingState,
}

/// A file to send with a queued message; `path` is read only during the call.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiOutgoingFile {
    pub path: String,
    /// The name as the user sees it.
    pub filename: String,
    /// Declared, untrusted.
    pub content_type: String,
    /// Your id for its progress and cancel (make one before calling, so the copy can be
    /// followed and cancelled while the call runs); nil: core makes one (see the receipt).
    pub transfer_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiQueuedFile {
    pub file_client_id: String,
    pub transfer_id: u64,
    pub size: u64,
}

/// What `send_queued_with_files` queued.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiSendReceipt {
    pub client_id: String,
    pub files: Vec<FfiQueuedFile>,
}

impl From<brook_core::SendReceipt> for FfiSendReceipt {
    fn from(r: brook_core::SendReceipt) -> Self {
        Self {
            client_id: r.client_id,
            files: r
                .files
                .into_iter()
                .map(|f| FfiQueuedFile {
                    file_client_id: f.file_client_id,
                    transfer_id: f.transfer_id.0,
                    size: f.size,
                })
                .collect(),
        }
    }
}

/// Where a transfer is.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiTransferState {
    /// Being copied into its encrypted snapshot (before any upload).
    Preparing,
    Running,
    /// Waiting before the next attempt (the server asked, the network failed, or signed out).
    Retrying {
        after_secs: u64,
    },
    Done,
    Cancelled,
    Failed {
        code: String,
    },
}

/// Progress of one transfer.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiTransferEvent {
    pub transfer_id: u64,
    pub done: u64,
    pub total: u64,
    pub state: FfiTransferState,
}

impl From<TransferEvent> for FfiTransferEvent {
    fn from(e: TransferEvent) -> Self {
        Self {
            transfer_id: e.id.0,
            done: e.done,
            total: e.total,
            state: match e.state {
                TransferState::Preparing => FfiTransferState::Preparing,
                TransferState::Running => FfiTransferState::Running,
                TransferState::Retrying { after_secs } => FfiTransferState::Retrying { after_secs },
                TransferState::Done => FfiTransferState::Done,
                TransferState::Cancelled => FfiTransferState::Cancelled,
                TransferState::Failed(code) => FfiTransferState::Failed { code },
            },
        }
    }
}

/// Implemented in Swift. Callbacks come one at a time from a runtime thread.
#[uniffi::export(with_foreign)]
pub trait TransferListener: Send + Sync {
    fn on_transfer(&self, event: FfiTransferEvent);
    /// Progress was missed (a slow listener): re-read `pending_messages` for the current
    /// state of every file, then carry on with the events that follow.
    fn on_resync(&self);
}

/// Deliver transfer events until cancelled or closed; missed ones become one `on_resync`.
pub(crate) fn deliver_transfers(
    mut rx: broadcast::Receiver<TransferEvent>,
    listener: Arc<dyn TransferListener>,
) -> Arc<Subscription> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    let task = runtime().spawn(async move {
        loop {
            let next = rx.recv().await;
            if flag.load(Ordering::SeqCst) {
                break;
            }
            match next {
                Ok(e) => listener.on_transfer(e.into()),
                Err(RecvError::Lagged(_)) => listener.on_resync(),
                Err(RecvError::Closed) => break,
            }
        }
    });
    Subscription::from_task(cancelled, task)
}

impl From<brook_core::PendingMessage> for FfiPendingMessage {
    fn from(p: brook_core::PendingMessage) -> Self {
        Self {
            client_id: p.client_id,
            channel_id: p.channel_id,
            body: p.body,
            reply_to_id: p.reply_to_id,
            files: p
                .files
                .into_iter()
                .map(|f| FfiPendingFile {
                    file_client_id: f.file_client_id,
                    transfer_id: f.transfer_id.0,
                    filename: f.filename,
                    size: f.size,
                    uploaded: f.uploaded,
                    error: f.error,
                })
                .collect(),
            state: match p.state {
                PendingState::Pending => FfiPendingState::Pending,
                PendingState::Sending => FfiPendingState::Sending,
                PendingState::Accepted => FfiPendingState::Accepted,
                PendingState::Failed { code } => FfiPendingState::Failed { code },
            },
        }
    }
}

/// What `delete_pending` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiDeleted {
    /// Removed before the server had it (as far as this device knows).
    Removed,
    /// The server had already accepted it: it's a sent message now.
    AlreadySent,
    NotFound,
}

impl From<Deleted> for FfiDeleted {
    fn from(d: Deleted) -> Self {
        match d {
            Deleted::Removed => Self::Removed,
            Deleted::AlreadySent => Self::AlreadySent,
            Deleted::NotFound => Self::NotFound,
        }
    }
}

/// Another user with data on this device.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiLocalUser {
    pub origin: String,
    pub user_id: String,
}

/// A change notice: re-read what it names. Hints only; nothing depends on receiving one.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiCacheEvent {
    /// Channels whose rows changed (the channel, its members, its messages).
    Channels { ids: Vec<String> },
    /// Channels the user was removed from: gone from the cache.
    Removed { ids: Vec<String> },
    /// Profiles that changed: re-render those authors.
    Users { ids: Vec<String> },
    /// Re-read everything (a server reset, or notices were missed).
    Reset,
    /// A channel's pending messages changed: re-read `pending_messages`.
    Outbox { channel_id: String },
    /// Unsent messages were lost: read `outbox_lost`.
    OutboxLost,
}

impl From<CacheEvent> for FfiCacheEvent {
    fn from(e: CacheEvent) -> Self {
        match e {
            CacheEvent::Channels(ids) => Self::Channels { ids },
            CacheEvent::Removed(ids) => Self::Removed { ids },
            CacheEvent::Users(ids) => Self::Users { ids },
            CacheEvent::Reset => Self::Reset,
            CacheEvent::Outbox(channel_id) => Self::Outbox { channel_id },
            CacheEvent::OutboxLost => Self::OutboxLost,
        }
    }
}

/// The signed-in user's sync state (the default while nobody's cache feeds it).
#[derive(Debug, Clone, PartialEq, Eq, Default, uniffi::Record)]
pub struct FfiCacheState {
    pub syncing: bool,
    /// Unix milliseconds of the last completed sync.
    pub last_synced_unix_ms: Option<i64>,
    /// The last sync couldn't reach the server (cached reads still work).
    pub offline: bool,
}

impl From<CacheState> for FfiCacheState {
    fn from(s: CacheState) -> Self {
        Self {
            syncing: s.syncing,
            last_synced_unix_ms: s
                .last_synced
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| i64::try_from(d.as_millis()).ok()),
            offline: s.offline,
        }
    }
}

/// Implemented in Swift. Callbacks come one at a time from a runtime thread.
#[uniffi::export(with_foreign)]
pub trait CacheEventListener: Send + Sync {
    fn on_cache_event(&self, event: FfiCacheEvent);
}

/// Implemented in Swift. Latest state wins (see `AuthStateListener`).
#[uniffi::export(with_foreign)]
pub trait CacheStateListener: Send + Sync {
    fn on_cache_state(&self, state: FfiCacheState);
}

/// Deliver `rx`'s notices until cancelled or closed. Missed notices (a slow listener)
/// become one `Reset`: re-reading everything covers whatever they were.
pub(crate) fn deliver_cache_events(
    mut rx: broadcast::Receiver<CacheEvent>,
    listener: Arc<dyn CacheEventListener>,
) -> Arc<Subscription> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    let task = runtime().spawn(async move {
        loop {
            let event = match rx.recv().await {
                Ok(e) => FfiCacheEvent::from(e),
                Err(RecvError::Lagged(_)) => FfiCacheEvent::Reset,
                Err(RecvError::Closed) => break,
            };
            if flag.load(Ordering::SeqCst) {
                break;
            }
            listener.on_cache_event(event);
        }
    });
    Subscription::from_task(cancelled, task)
}

#[uniffi::export]
impl FfiBrookClient {
    /// Turn on the encrypted cache and outbox, keyed in `slot` (the same Keychain slot
    /// store as `enable_persistence`), under `data_dir`. False: local data is off (a locked
    /// or damaged key store; nothing deleted) and the app works online-only.
    pub async fn enable_local_data(
        &self,
        slot: Arc<dyn crate::keyslot::FfiKeySlot>,
        data_dir: String,
    ) -> bool {
        let inner = Arc::clone(&self.inner);
        let slot = Arc::new(crate::keyslot::SlotAdapter(slot));
        runtime()
            .spawn(async move {
                inner
                    .enable_local_data(slot, std::path::PathBuf::from(data_dir))
                    .await
            })
            .await
            .unwrap_or(false)
    }

    /// Change notices from the cache and outbox (see [`CacheEventListener`]).
    pub fn subscribe_cache_events(
        &self,
        listener: Arc<dyn CacheEventListener>,
    ) -> Arc<Subscription> {
        deliver_cache_events(self.inner.cache_events(), listener)
    }

    /// The signed-in user's sync state, current value first (see [`CacheStateListener`]).
    pub fn subscribe_cache_state(
        &self,
        listener: Arc<dyn CacheStateListener>,
    ) -> Arc<Subscription> {
        subscribe_watch(self.inner.subscribe_cache_state(), move |s: CacheState| {
            listener.on_cache_state(s.into())
        })
    }

    /// The newest loss of unsent messages not yet acknowledged, or nil. Read at start and
    /// after `OutboxLost` or `Reset`; tell the user, then acknowledge that number.
    pub fn outbox_lost(&self) -> Option<u64> {
        self.inner.outbox_lost()
    }

    /// The user was told about loss `n`; a newer loss stays reported.
    pub fn acknowledge_outbox_lost(&self, n: u64) {
        self.inner.acknowledge_outbox_lost(n)
    }

    /// Cached channels of the signed-in user, with unread counts computed locally.
    pub async fn cached_channels(&self) -> Result<Vec<FfiCachedChannel>, LoginError> {
        let inner = Arc::clone(&self.inner);
        let channels = run(async move { inner.cached_channels().await }).await?;
        Ok(channels.into_iter().map(Into::into).collect())
    }

    /// Cached messages, newest first, older than `before` if given. Never touches the
    /// network.
    pub async fn cached_messages(
        &self,
        channel_id: String,
        before: Option<String>,
        limit: u32,
    ) -> Result<FfiCachedMessages, LoginError> {
        let inner = Arc::clone(&self.inner);
        let page = run(async move {
            inner
                .cached_messages(&channel_id, before.as_deref(), limit as usize)
                .await
        })
        .await?;
        Ok(FfiCachedMessages {
            messages: page.messages.into_iter().map(Into::into).collect(),
            needs_network: page.needs_network,
        })
    }

    /// Fetch the newest page of a channel into the cache.
    pub async fn load_head(&self, channel_id: String, limit: u32) -> Result<(), LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.load_head(&channel_id, limit as usize).await }).await
    }

    /// Fetch the page below what's cached into the cache.
    pub async fn load_older(&self, channel_id: String, limit: u32) -> Result<(), LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.load_older(&channel_id, limit as usize).await }).await
    }

    /// Queue a message: saved before this returns, sent in order when possible.
    /// `client_id` is required: make it (a UUID) and keep it **before** calling, so a
    /// call whose answer was lost can be retried with the same id. Any case is accepted;
    /// the returned id is the canonical lowercase form that `pending_messages` and the sent
    /// message's `client_id` use, so match bubbles on that. Not a UUID: `outbox.bad_id`.
    /// `reply_to_id`: the quoted message, for a reply. The same `client_id` again returns
    /// the stored message, reply target included: quoting something else needs a new id.
    pub async fn send_queued(
        &self,
        channel_id: String,
        body: String,
        reply_to_id: Option<String>,
        client_id: String,
    ) -> Result<String, LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move {
            inner
                .send_queued(&channel_id, &body, reply_to_id, Some(client_id))
                .await
        })
        .await
    }

    /// Queue a message with files (at most `max_files_per_message()`, each at most
    /// `max_file_bytes()`, none empty; the body may be empty). Each file is copied into an
    /// encrypted snapshot before this returns, so its path is read only now; call it off the
    /// main thread. Progress (`Preparing`, then the upload) arrives on the receipt's transfer
    /// ids; `cancel_transfer` on any of them cancels the message's sending. `client_id` is
    /// required, as for `send_queued`; the same id again returns the stored receipt.
    pub async fn send_queued_with_files(
        &self,
        channel_id: String,
        body: String,
        reply_to_id: Option<String>,
        client_id: String,
        files: Vec<FfiOutgoingFile>,
    ) -> Result<FfiSendReceipt, LoginError> {
        let inner = Arc::clone(&self.inner);
        let files = files
            .into_iter()
            .map(|f| brook_core::OutgoingFile {
                path: std::path::PathBuf::from(f.path),
                filename: f.filename,
                content_type: f.content_type,
                transfer_id: f.transfer_id.map(TransferId),
            })
            .collect();
        let receipt = run(async move {
            inner
                .send_queued_with_files(&channel_id, &body, reply_to_id, Some(client_id), files)
                .await
        })
        .await?;
        Ok(receipt.into())
    }

    /// Transfer progress of this client (filter by transfer id).
    pub fn subscribe_transfers(&self, listener: Arc<dyn TransferListener>) -> Arc<Subscription> {
        deliver_transfers(self.inner.transfer_events(), listener)
    }

    /// Stop a transfer; for a queued message's file, cancels the message's sending (Retry
    /// resumes it).
    pub fn cancel_transfer(&self, transfer_id: u64) {
        self.inner.cancel_transfer(TransferId(transfer_id));
    }

    /// A channel's messages that haven't gone out, in the order they will.
    pub async fn pending_messages(
        &self,
        channel_id: String,
    ) -> Result<Vec<FfiPendingMessage>, LoginError> {
        let inner = Arc::clone(&self.inner);
        let pending = run(async move { inner.pending_messages(&channel_id).await }).await?;
        Ok(pending.into_iter().map(Into::into).collect())
    }

    /// Put a failed message back in line.
    pub async fn retry_send(&self, client_id: String) -> Result<(), LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.retry_send(&client_id).await }).await
    }

    /// Retry a failed reply without its quote (after `message.reply_target_gone`), keeping
    /// its place in the queue.
    pub async fn retry_without_reply(&self, client_id: String) -> Result<(), LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.retry_without_reply(&client_id).await }).await
    }

    /// Remove a message that hasn't gone out.
    pub async fn delete_pending(&self, client_id: String) -> Result<FfiDeleted, LoginError> {
        let inner = Arc::clone(&self.inner);
        Ok(run(async move { inner.delete_pending(&client_id).await })
            .await?
            .into())
    }

    /// Unsent messages of the signed-in user (the sign-out warning).
    pub async fn unsent_count(&self) -> u64 {
        let inner = Arc::clone(&self.inner);
        runtime()
            .spawn(async move { inner.unsent_count().await })
            .await
            .unwrap_or(0)
    }

    /// Other users with data on this device.
    pub async fn other_local_users(&self) -> Result<Vec<FfiLocalUser>, LoginError> {
        let inner = Arc::clone(&self.inner);
        let users = run(async move { inner.other_local_users().await }).await?;
        Ok(users
            .into_iter()
            .map(|(origin, user_id)| FfiLocalUser { origin, user_id })
            .collect())
    }

    /// Erase every other user's data on this device.
    pub async fn wipe_other_local_users(&self) -> Result<(), LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.wipe_other_local_users().await }).await
    }

    /// "Remove this device's data", then sign out. The erase is local and happens first;
    /// an error means some of it couldn't be erased (the sign-out still happened).
    pub async fn sign_out_and_forget(&self) -> Result<(), LoginError> {
        let inner = Arc::clone(&self.inner);
        run(async move { inner.sign_out_and_forget().await }).await
    }
}

/// At most this many files per message.
#[uniffi::export]
pub fn max_files_per_message() -> u32 {
    brook_core::MAX_FILES_PER_MESSAGE as u32
}

/// Each file at most this many bytes (the server's default).
#[uniffi::export]
pub fn max_file_bytes() -> u64 {
    brook_core::MAX_FILE_BYTES
}

#[cfg(test)]
#[path = "offline_tests.rs"]
mod tests;
