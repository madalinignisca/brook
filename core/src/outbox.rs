//! The outbox (plan C4, #63; spec §5): messages written while offline (or before the server
//! answered) are durable, go out in the order written, once, and are never dropped
//! silently.
//!
//! - **Queued means durable:** `enqueue` commits the row before it returns. The caller may
//!   pass its own `client_id`, so a retry after an error (or a cancelled call) is the same
//!   message, never a second one.
//! - **One sender per channel, by ordinal.** A failed row blocks only its own channel.
//! - **Ack order:** the returned message is applied to the cache and committed first, then
//!   the row whose `client_id` equals the *echoed* one is deleted. If the cache can't take
//!   it, the row is kept as `accepted` (the server has it): it's resent with the same
//!   `client_id`, which the server answers with the stored message.
//! - **Status table:** unreachable, 5xx, 408, 429 and 401 stay pending (retried after
//!   `Retry-After` or a backoff; for a 401 the refresh loop renews the token meanwhile); any
//!   other refusal fails the row with the server's code (Retry and Delete). A store that
//!   can't record the outcome backs off too, never re-sends in a tight loop.
//! - **Signed out means paused**, however the session ended. The session is checked right
//!   before each POST and handed to it (`Post` refuses a session that isn't the current
//!   one); an answer that arrives after the session changed is discarded (the row stays).
//! - Retry, Delete and direct sends go through the channel sender's lock: a Delete of a row
//!   in flight waits for that attempt, and one the server took reports `AlreadySent`.
//! - Every idle wait is bounded, so a wake-up lost to a cancelled call or a store that came
//!   back is only a delay.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{watch, Mutex, Notify};

use crate::cache::Cache;
use crate::snapshot::{self, SnapshotSource};
use crate::store::{Db, StoreError};
use crate::transfer::{FileInfo, Flags, TransferId, TransferState, Transfers};

/// At most this many files per message (the server's limit).
pub const MAX_FILES_PER_MESSAGE: usize = 10;
/// Each file at most this big: the server's default `files_max_bytes`. A server configured
/// lower refuses at upload (413, the row fails); one configured higher is capped here until
/// the server publishes its limits.
pub const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// A file to send with a queued message. `path` is read only during the call (a portal path
/// may be readable once): it's copied into an encrypted snapshot before the call returns.
#[derive(Debug, Clone)]
pub struct OutgoingFile {
    pub path: PathBuf,
    /// The name as the user sees it (display text; the server sanitises what it stores).
    pub filename: String,
    /// Declared, untrusted.
    pub content_type: String,
}

/// What `send_queued_with_files` queued: the message's id and each file's, with the transfer
/// id its progress arrives under (`transfer_events`) and that cancels it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReceipt {
    pub client_id: String,
    pub files: Vec<QueuedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedFile {
    pub file_client_id: String,
    pub transfer_id: TransferId,
    pub size: u64,
}

/// A queued message's file, as the UI draws it (also after a restart: transfer ids are per
/// process, and `pending_messages` gives the current ones).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFile {
    pub file_client_id: String,
    pub transfer_id: TransferId,
    pub filename: String,
    pub size: u64,
    /// On the server already (its upload finished).
    pub uploaded: bool,
    /// The server's refusal of this file, if it's the one that failed the message.
    pub error: Option<String>,
}

/// A queued file's row.
#[derive(Debug, Clone)]
pub(crate) struct FileRow {
    pub(crate) file_client_id: String,
    pub(crate) filename: String,
    pub(crate) content_type: String,
    pub(crate) size: u64,
    pub(crate) sha256: String,
    pub(crate) key: [u8; 32],
    /// The chunk size its snapshot was written with (a later change of `CHUNK` can't
    /// misread it).
    pub(crate) chunk: usize,
    pub(crate) file_id: Option<String>,
}

/// Where a queued file's bytes go (`transfer.rs`'s upload, under the sender's session
/// `epoch` only: an implementation refuses with `NotAuthenticated` once the signed-in
/// session isn't that one, so a snapshot is never uploaded as another user).
#[async_trait::async_trait]
pub(crate) trait Upload: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn upload(
        &self,
        id: TransferId,
        flags: &Arc<Flags>,
        channel_id: &str,
        file: &FileRow,
        source: &SnapshotSource,
        epoch: u64,
    ) -> Result<FileInfo, crate::Error>;
}

/// What a row sends. A struct, not more parameters: attachments added a field.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Outgoing {
    pub(crate) body: String,
    /// The quoted message, for a reply.
    pub(crate) reply_to_id: Option<String>,
    /// The uploaded files' ids, in the order the user gave them.
    pub(crate) attachments: Vec<String>,
}

/// Where a sent message goes (`POST /channels/{id}/messages`). `epoch` is the session the
/// sender checked: an implementation refuses to send (`Transient`) if the signed-in session
/// is no longer that one, so a message never goes out under another session.
#[async_trait::async_trait]
pub(crate) trait Post: Send + Sync {
    async fn send(
        &self,
        channel_id: &str,
        msg: &Outgoing,
        client_id: &str,
        epoch: u64,
    ) -> Result<Value, SendFailure>;
}

/// How a send attempt failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendFailure {
    /// Try again later: unreachable, 5xx, 408, 429, 401 (`retry_after`, if the server gave
    /// one).
    Transient { retry_after: Option<u64> },
    /// Refused for good: the row fails with this code (the user may Retry or Delete).
    Refused { code: String },
}

/// The session the sender works for: `Some(epoch)` while signed in (a new sign-in is a new
/// epoch), `None` while signed out.
pub(crate) type Session = watch::Receiver<Option<u64>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingState {
    Pending,
    Sending,
    /// The server has it; the cache hasn't caught up (it's resent to get it back).
    Accepted,
    Failed {
        code: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMessage {
    pub client_id: String,
    pub channel_id: String,
    pub body: String,
    /// The quoted message, for a reply ("Replying to …").
    pub reply_to_id: Option<String>,
    /// Its files, in order (empty for a text message).
    pub files: Vec<PendingFile>,
    pub state: PendingState,
}

/// What `delete_pending` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    /// Removed before the server had it (as far as core knows: an answer lost on the way
    /// can't be known).
    Removed,
    /// The server had already accepted it: it's a sent message (sends can't be revoked).
    AlreadySent,
    /// No such pending message.
    NotFound,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OutboxError {
    /// The outbox can't be written (disk full) and this channel has messages waiting: a
    /// direct send could overtake them, so it's refused.
    #[error("sending is paused until earlier messages go out")]
    WouldOvertake,
    /// Signed out: nothing is sent, and the outbox can't take it either.
    #[error("not signed in")]
    SignedOut,
    #[error("local storage failed")]
    Store,
    /// A direct send (outbox unwritable) that didn't go through: send it again with the
    /// same `client_id` (the server keeps one message per `client_id`).
    #[error("not sent ({reason})")]
    NotSent { client_id: String, reason: String },
    #[error("the outbox is closed")]
    Closed,
    /// This `client_id` is already queued for another channel.
    #[error("that message id is already used")]
    IdInUse,
    /// This `client_id` isn't a UUID (the server would refuse it).
    #[error("the message id isn't a UUID")]
    BadId,
    /// More than [`MAX_FILES_PER_MESSAGE`] files.
    #[error("too many files for one message")]
    TooManyFiles,
    /// A file over [`MAX_FILE_BYTES`].
    #[error("a file is too large")]
    FileTooLarge,
    /// An empty file (the server refuses one).
    #[error("a file is empty")]
    EmptyFile,
    /// Neither text nor files.
    #[error("the message is empty")]
    EmptyMessage,
    /// A file couldn't be read to copy it (gone, or no permission).
    #[error("a file couldn't be read")]
    FileUnreadable,
    /// Cancelled while its files were being copied: nothing was queued.
    #[error("cancelled")]
    Cancelled,
}

/// Transient failures back off up to this.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// No idle wait is longer than this: a lost wake-up (a cancelled call, a store that came
/// back) costs at most this delay.
const IDLE_POLL: Duration = Duration::from_secs(30);

pub(crate) struct Outbox {
    db: Db,
    cache: Arc<Cache>,
    post: Arc<dyn Post>,
    upload: Arc<dyn Upload>,
    transfers: Arc<Transfers>,
    /// `<store>/snap`: one encrypted snapshot per queued file, named by its `file_client_id`.
    snap_dir: PathBuf,
    /// Snapshot chunk size (`snapshot::CHUNK`; tests use small ones, same format).
    chunk: AtomicUsize,
    /// Transfer ids, one per file per process (the UI's handle for progress and cancel).
    ids: StdMutex<HashMap<String, TransferId>>,
    /// Per row: the flags every transfer of it shares (registered in `transfers` too).
    row_flags: StdMutex<HashMap<String, Arc<Flags>>>,
    /// Files whose snapshot was verified in this process (cleared on a read error).
    verified: StdMutex<HashSet<String>>,
    session: Session,
    channels: StdMutex<HashMap<String, Arc<ChannelSender>>>,
    tasks: StdMutex<Vec<tokio::task::JoinHandle<()>>>,
    closed: AtomicBool,
    /// Where change notices go (`CacheEvent::Outbox`), once set.
    events: std::sync::OnceLock<tokio::sync::broadcast::Sender<crate::cache::CacheEvent>>,
}

/// One channel's sender: its lock (held for a whole attempt, and by Retry, Delete and direct
/// sends) and its wake-up.
struct ChannelSender {
    lock: Mutex<()>,
    wake: Notify,
}

/// A `client_id` in the form the server echoes it: the server parses it as a UUID and
/// answers with the canonical lowercase hyphenated text, and the echo is compared exactly,
/// so anything else (an uppercase Swift `UUID().uuidString`) would never match. Accepts the
/// hyphenated 8-4-4-4-12 form or 32 hex digits, any case; `None` for anything else.
pub(crate) fn canonical_client_id(id: &str) -> Option<String> {
    let hex: String = match id.len() {
        36 => {
            let dashes_ok = id
                .char_indices()
                .all(|(i, c)| matches!(i, 8 | 13 | 18 | 23) == (c == '-'));
            if !dashes_ok {
                return None;
            }
            id.chars().filter(|&c| c != '-').collect()
        }
        32 => id.to_string(),
        _ => return None,
    };
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let h = hex.to_ascii_lowercase();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    ))
}

pub(crate) fn new_client_id() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("OS randomness");
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn lock<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Outbox {
    /// Open over the outbox store. Rows a crash left `sending` go back to `pending`: they
    /// are resent with the same `client_id`, and the server answers a duplicate with the
    /// stored message.
    ///
    /// Before it returns (so before any enqueue can start a snapshot), the snapshot directory
    /// is reconciled: journalled deletions are finished and snapshots no row names are
    /// removed. That cleanup is best-effort: a file that won't go is left for next time and
    /// never stops the outbox opening.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open(
        db: Db,
        cache: Arc<Cache>,
        post: Arc<dyn Post>,
        upload: Arc<dyn Upload>,
        transfers: Arc<Transfers>,
        session: Session,
        store_dir: &Path,
    ) -> Result<Arc<Self>, StoreError> {
        let snap_dir = store_dir.join("snap");
        std::fs::create_dir_all(&snap_dir).map_err(|_| StoreError::Io)?;
        let (journal, live): (Vec<String>, HashSet<String>) = db
            .call(|c| {
                c.execute(
                    "UPDATE outbox SET state = 'pending' WHERE state = 'sending'",
                    [],
                )?;
                let journal = c
                    .prepare("SELECT path FROM deletions")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                let live = c
                    .prepare("SELECT file_client_id FROM outbox_files")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<HashSet<String>>>()?;
                Ok((journal, live))
            })
            .await?;
        let dir = snap_dir.clone();
        let done = tokio::task::spawn_blocking(move || {
            let mut done = vec![];
            for name in &journal {
                match std::fs::remove_file(dir.join(name)) {
                    Ok(()) => done.push(name.clone()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => done.push(name.clone()),
                    Err(_) => {} // left for next time
                }
            }
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if !live.contains(&name) {
                        let _ = std::fs::remove_file(e.path()); // an orphan: no row names it
                    }
                }
            }
            done
        })
        .await
        .unwrap_or_default();
        let _ = db
            .call(move |c| {
                for name in &done {
                    c.execute("DELETE FROM deletions WHERE path = ?1", [name])?;
                }
                Ok(())
            })
            .await;
        Ok(Arc::new(Self {
            db,
            cache,
            post,
            upload,
            transfers,
            snap_dir,
            chunk: AtomicUsize::new(snapshot::CHUNK),
            ids: StdMutex::default(),
            row_flags: StdMutex::default(),
            verified: StdMutex::default(),
            session,
            channels: StdMutex::default(),
            tasks: StdMutex::default(),
            closed: AtomicBool::new(false),
            events: std::sync::OnceLock::new(),
        }))
    }

    /// The store, for tests that make it fail (triggers).
    #[cfg(test)]
    pub(crate) fn db_for_tests(&self) -> &Db {
        &self.db
    }

    /// Smaller snapshot chunks (tests: the same format, fewer bytes).
    #[cfg(test)]
    pub(crate) fn set_chunk_for_tests(&self, chunk: usize) {
        self.chunk.store(chunk, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn snap_dir_for_tests(&self) -> &Path {
        &self.snap_dir
    }

    /// Send change notices to `events` (`CacheEvent::Outbox(channel)` on every change).
    pub(crate) fn set_events(
        &self,
        events: tokio::sync::broadcast::Sender<crate::cache::CacheEvent>,
    ) {
        let _ = self.events.set(events);
    }

    fn changed(&self, channel_id: &str) {
        if let Some(events) = self.events.get() {
            let _ = events.send(crate::cache::CacheEvent::Outbox(channel_id.to_string()));
        }
    }

    fn check_open(&self) -> Result<(), OutboxError> {
        if self.closed.load(Ordering::SeqCst) {
            Err(OutboxError::Closed)
        } else {
            Ok(())
        }
    }

    fn sender(self: &Arc<Self>, channel_id: &str) -> Result<Arc<ChannelSender>, OutboxError> {
        let mut channels = lock(&self.channels);
        self.check_open()?; // under the map's lock: `close` takes it too
        if let Some(s) = channels.get(channel_id) {
            return Ok(s.clone());
        }
        let s = Arc::new(ChannelSender {
            lock: Mutex::new(()),
            wake: Notify::new(),
        });
        channels.insert(channel_id.to_string(), s.clone());
        let (me, id, sender) = (Arc::clone(self), channel_id.to_string(), s.clone());
        let task = tokio::spawn(async move { me.run_channel(&id, &sender).await });
        lock(&self.tasks).push(task);
        Ok(s)
    }

    /// Stop every sender and close the store (sign-out wipes, shutdown). A sender stopped
    /// mid-send is like a crash: its row stays `sending`, and the next `open` resends it
    /// with the same `client_id`. Afterwards every call answers `Closed`.
    pub(crate) async fn close(self: Arc<Self>) {
        {
            let mut channels = lock(&self.channels);
            self.closed.store(true, Ordering::SeqCst);
            channels.clear();
        }
        let tasks: Vec<_> = std::mem::take(&mut *lock(&self.tasks));
        for t in &tasks {
            t.abort();
        }
        for t in tasks {
            let _ = t.await;
        }
        // This user's transfer ids leave the client's registry with them.
        let ids: Vec<TransferId> = lock(&self.ids).drain().map(|(_, id)| id).collect();
        self.transfers.unregister(&ids);
        lock(&self.row_flags).clear();
        // Closed through this handle, whoever else holds one (a Retry or Delete in flight
        // gets `Closed`): nothing reaches the file after this returns.
        self.db.close().await;
    }

    /// Start the senders of every channel with rows waiting (at startup, after `open`).
    pub(crate) async fn resume(self: &Arc<Self>) -> Result<(), OutboxError> {
        let channels: Vec<String> = self
            .db
            .call(|c| {
                c.prepare("SELECT DISTINCT channel_id FROM outbox")?
                    .query_map([], |r| r.get(0))?
                    .collect()
            })
            .await
            .map_err(|_| OutboxError::Store)?;
        for id in channels {
            self.sender(&id)?.wake.notify_one();
        }
        Ok(())
    }

    /// Queue a message: durable (committed) before this returns. Pass the `client_id` of an
    /// earlier attempt to retry it (never a second message): the stored row wins, body and
    /// reply target included, so a different target needs a new id. If the outbox can't
    /// be written, it's sent directly, only where nothing is waiting.
    pub(crate) async fn enqueue(
        self: &Arc<Self>,
        channel_id: &str,
        body: &str,
        reply_to_id: Option<String>,
        client_id: Option<String>,
    ) -> Result<String, OutboxError> {
        let client_id = match client_id {
            Some(id) => canonical_client_id(&id).ok_or(OutboxError::BadId)?,
            None => new_client_id(),
        };
        let sender = self.sender(channel_id)?;
        // Woken before the insert, not after: a caller cancelled mid-insert leaves a
        // committed row, and the sender must still look (a spare wake-up is harmless).
        sender.wake.notify_one();
        let msg = Outgoing {
            body: body.to_string(),
            reply_to_id,
            attachments: vec![],
        };
        let (ch, m, cid) = (channel_id.to_string(), msg.clone(), client_id.clone());
        let queued = self
            .db
            .call(move |c| {
                c.execute(
                    "INSERT INTO outbox(client_id, channel_id, body, reply_to_id, state, created_at)
                     VALUES (?1, ?2, ?3, ?4, 'pending', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                     ON CONFLICT(client_id) DO NOTHING",
                    rusqlite::params![cid, ch, m.body, m.reply_to_id],
                )
            })
            .await;
        match queued {
            Ok(0) => {
                // The id is taken: the same message again (the stored one wins), unless it
                // was queued for another channel, where it would never be sent.
                let cid = client_id.clone();
                let theirs: String = self
                    .db
                    .call(move |c| {
                        c.query_row(
                            "SELECT channel_id FROM outbox WHERE client_id = ?1",
                            [&cid],
                            |r| r.get(0),
                        )
                    })
                    .await
                    .map_err(|_| OutboxError::Store)?;
                if theirs != channel_id {
                    return Err(OutboxError::IdInUse);
                }
                sender.wake.notify_one();
                Ok(client_id)
            }
            Ok(_) => {
                sender.wake.notify_one();
                self.changed(channel_id);
                Ok(client_id)
            }
            Err(_) => self.send_direct(&sender, channel_id, &msg, client_id).await,
        }
    }

    /// The outbox couldn't take the row (disk full): send now, not queued, only if this
    /// channel has nothing waiting, and only while signed in. Under the channel's lock, so
    /// neither another direct send nor the queue can overtake it.
    async fn send_direct(
        &self,
        sender: &ChannelSender,
        channel_id: &str,
        msg: &Outgoing,
        client_id: String,
    ) -> Result<String, OutboxError> {
        let _held = sender.lock.lock().await;
        self.check_open()?; // closed while waiting for the lock
        let Some(epoch) = *self.session.borrow() else {
            return Err(OutboxError::SignedOut);
        };
        let ch = channel_id.to_string();
        let waiting: i64 = self
            .db
            .call(move |c| {
                c.query_row(
                    "SELECT count(*) FROM outbox WHERE channel_id = ?1",
                    [&ch],
                    |r| r.get(0),
                )
            })
            .await
            .map_err(|_| OutboxError::Store)?;
        if waiting > 0 {
            return Err(OutboxError::WouldOvertake);
        }
        match self.post.send(channel_id, msg, &client_id, epoch).await {
            Ok(message) => {
                let _ = self.cache.apply_ack(&message).await; // /sync brings it otherwise
                Ok(client_id)
            }
            Err(SendFailure::Refused { code }) => Err(OutboxError::NotSent {
                client_id,
                reason: code,
            }),
            Err(SendFailure::Transient { .. }) => Err(OutboxError::NotSent {
                client_id,
                reason: "network".into(),
            }),
        }
    }

    /// The channel's waiting messages, in the order they will go, with their files.
    pub(crate) async fn pending(
        &self,
        channel_id: &str,
    ) -> Result<Vec<PendingMessage>, StoreError> {
        let ch = channel_id.to_string();
        let (mut rows, files) = self
            .db
            .call(move |c| {
                let rows = c
                    .prepare(
                        "SELECT client_id, channel_id, body, state, error, reply_to_id FROM outbox
                         WHERE channel_id = ?1 ORDER BY ordinal",
                    )?
                    .query_map([&ch], |r| {
                        let state: String = r.get(3)?;
                        Ok(PendingMessage {
                            client_id: r.get(0)?,
                            channel_id: r.get(1)?,
                            body: r.get(2)?,
                            reply_to_id: r.get(5)?,
                            files: vec![],
                            state: match state.as_str() {
                                "sending" => PendingState::Sending,
                                "accepted" => PendingState::Accepted,
                                "failed" => PendingState::Failed {
                                    code: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                                },
                                _ => PendingState::Pending,
                            },
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<PendingMessage>>>()?;
                let files = c
                    .prepare(
                        "SELECT f.client_id, f.file_client_id, f.filename, f.size,
                                f.file_id IS NOT NULL, f.error
                         FROM outbox_files f JOIN outbox o ON o.client_id = f.client_id
                         WHERE o.channel_id = ?1 ORDER BY f.client_id, f.ordinal",
                    )?
                    .query_map([&ch], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, bool>(4)?,
                            r.get::<_, Option<String>>(5)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok((rows, files))
            })
            .await?;
        for (row_id, fcid, filename, size, uploaded, error) in files {
            let transfer_id = self.transfer_id(&row_id, &fcid);
            if let Some(m) = rows.iter_mut().find(|m| m.client_id == row_id) {
                m.files.push(PendingFile {
                    file_client_id: fcid,
                    transfer_id,
                    filename,
                    size: size as u64,
                    uploaded,
                    error,
                });
            }
        }
        Ok(rows)
    }

    /// The transfer id of a row's file in this process (issued on first use), registered
    /// under the row's flags so cancelling it stops the row.
    fn transfer_id(&self, row_id: &str, file_client_id: &str) -> TransferId {
        let id = *lock(&self.ids)
            .entry(file_client_id.to_string())
            .or_default();
        let flags = self.flags(row_id);
        self.transfers.register(&[id], &flags);
        id
    }

    /// The row's flags (made on first use).
    fn flags(&self, row_id: &str) -> Arc<Flags> {
        lock(&self.row_flags)
            .entry(row_id.to_string())
            .or_default()
            .clone()
    }

    /// A call's own files, never committed (a failed copy, or a repeat that lost the race):
    /// their ids go, but not the row's flags, which a live row with this id may be using.
    fn drop_ids(&self, file_client_ids: &[String]) {
        let ids: Vec<TransferId> = {
            let mut map = lock(&self.ids);
            file_client_ids
                .iter()
                .filter_map(|f| map.remove(f))
                .collect()
        };
        self.transfers.unregister(&ids);
    }

    /// The row is gone: its registrations and per-process marks go with it.
    fn forget_row(&self, row_id: &str, file_client_ids: &[String]) {
        lock(&self.row_flags).remove(row_id);
        let ids: Vec<TransferId> = {
            let mut map = lock(&self.ids);
            file_client_ids
                .iter()
                .filter_map(|f| map.remove(f))
                .collect()
        };
        self.transfers.unregister(&ids);
        let mut verified = lock(&self.verified);
        for f in file_client_ids {
            verified.remove(f);
        }
    }

    async fn files_of(&self, row_id: &str) -> Result<Vec<FileRow>, StoreError> {
        let id = row_id.to_string();
        self.db
            .call(move |c| {
                c.prepare(
                    "SELECT file_client_id, filename, content_type, size, sha256, key, file_id,
                            chunk
                     FROM outbox_files WHERE client_id = ?1 ORDER BY ordinal",
                )?
                .query_map([&id], |r| {
                    let key: Vec<u8> = r.get(5)?;
                    Ok(FileRow {
                        file_client_id: r.get(0)?,
                        filename: r.get(1)?,
                        content_type: r.get(2)?,
                        size: r.get::<_, i64>(3)? as u64,
                        sha256: r.get(4)?,
                        key: key.try_into().unwrap_or([0u8; 32]),
                        file_id: r.get(6)?,
                        chunk: r.get::<_, i64>(7)? as usize,
                    })
                })?
                .collect()
            })
            .await
    }

    /// The receipt of a stored row (a repeat call returns it).
    async fn receipt_of(&self, row_id: &str) -> Result<SendReceipt, OutboxError> {
        let files = self
            .files_of(row_id)
            .await
            .map_err(|_| OutboxError::Store)?;
        Ok(SendReceipt {
            client_id: row_id.to_string(),
            files: files
                .into_iter()
                .map(|f| QueuedFile {
                    transfer_id: self.transfer_id(row_id, &f.file_client_id),
                    file_client_id: f.file_client_id,
                    size: f.size,
                })
                .collect(),
        })
    }

    /// Queue a message with files. Limits are checked before anything is copied; each file
    /// is snapshotted (encrypted, fsynced) before the row commits, so a queued file can't be
    /// changed or lost by what happens to its source afterwards. The same `client_id` again
    /// returns the stored row's receipt (checked before copying). Files need the outbox: if
    /// it can't be written, this fails (`Store`) rather than sending anything directly.
    pub(crate) async fn enqueue_with_files(
        self: &Arc<Self>,
        channel_id: &str,
        body: &str,
        reply_to_id: Option<String>,
        client_id: Option<String>,
        files: Vec<OutgoingFile>,
    ) -> Result<SendReceipt, OutboxError> {
        if files.len() > MAX_FILES_PER_MESSAGE {
            return Err(OutboxError::TooManyFiles);
        }
        if files.is_empty() {
            if body.trim().is_empty() {
                return Err(OutboxError::EmptyMessage);
            }
            let client_id = self
                .enqueue(channel_id, body, reply_to_id, client_id)
                .await?;
            return Ok(SendReceipt {
                client_id,
                files: vec![],
            });
        }
        for f in &files {
            let len = std::fs::metadata(&f.path)
                .map_err(|_| OutboxError::FileUnreadable)?
                .len();
            if len == 0 {
                return Err(OutboxError::EmptyFile);
            }
            if len > MAX_FILE_BYTES {
                return Err(OutboxError::FileTooLarge);
            }
        }
        let client_id = match client_id {
            Some(id) => canonical_client_id(&id).ok_or(OutboxError::BadId)?,
            None => new_client_id(),
        };
        self.check_open()?;
        if let Some(theirs) = self.row_channel(&client_id).await? {
            if theirs != channel_id {
                return Err(OutboxError::IdInUse);
            }
            return self.receipt_of(&client_id).await;
        }
        // The channel's sender exists before anything commits (as for `enqueue`): a caller
        // dropped after the commit must not leave a row no sender will look at.
        let sender = self.sender(channel_id)?;
        // Snapshots first (off the runtime's workers), each under its own new key.
        let chunk = self.chunk.load(Ordering::SeqCst);
        let row_flags = self.flags(&client_id);
        let mut made: Vec<(OutgoingFile, String, snapshot::Written)> = vec![];
        let mut failed = None;
        for f in files {
            let fcid = new_client_id();
            let tid = self.transfer_id(&client_id, &fcid);
            let (src, dst) = (f.path.clone(), self.snap_dir.join(&fcid));
            let id = snapshot::id_bytes(&fcid).expect("a canonical id");
            let (transfers, flags) = (self.transfers.clone(), row_flags.clone());
            let written = tokio::task::spawn_blocking(move || {
                snapshot::write(&src, &dst, id, chunk, &mut |done, total| {
                    transfers.emit(tid, done, total, TransferState::Preparing);
                    // A cancel while copying stops it: nothing is queued.
                    !flags.cancel.load(Ordering::SeqCst)
                })
            })
            .await;
            match written {
                Ok(Ok(w)) => made.push((f, fcid, w)),
                Ok(Err(snapshot::WriteError::Source)) => {
                    failed = Some(OutboxError::FileUnreadable);
                    break;
                }
                Ok(Err(snapshot::WriteError::Stopped)) => {
                    failed = Some(OutboxError::Cancelled);
                    break;
                }
                // Our side: a full disk, a store that can't be written.
                Ok(Err(snapshot::WriteError::Store)) | Err(_) => {
                    failed = Some(OutboxError::Store);
                    break;
                }
            }
        }
        let names: Vec<String> = made.iter().map(|(_, n, _)| n.clone()).collect();
        if let Some(err) = failed {
            self.remove_snapshots(&names).await;
            self.drop_ids(&names);
            if matches!(err, OutboxError::Cancelled) {
                row_flags.cancel.store(false, Ordering::SeqCst); // this id may be sent again
            }
            return Err(err);
        }
        let rows: Vec<(String, String, String, i64, String, Vec<u8>)> = made
            .iter()
            .map(|(f, fcid, w)| {
                (
                    fcid.clone(),
                    f.filename.clone(),
                    f.content_type.clone(),
                    w.size as i64,
                    w.sha256.clone(),
                    w.key.to_vec(),
                )
            })
            .collect();
        let chunk_col = chunk as i64;
        let (ch, b, cid) = (channel_id.to_string(), body.to_string(), client_id.clone());
        let inserted = self
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let n = tx.execute(
                    "INSERT INTO outbox(client_id, channel_id, body, reply_to_id, state, created_at)
                     VALUES (?1, ?2, ?3, ?4, 'pending', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                     ON CONFLICT(client_id) DO NOTHING",
                    rusqlite::params![cid, ch, b, reply_to_id],
                )?;
                if n == 1 {
                    for (i, (fcid, name, ctype, size, sha, key)) in rows.iter().enumerate() {
                        tx.execute(
                            "INSERT INTO outbox_files(client_id, ordinal, file_client_id,
                                 filename, content_type, size, sha256, key, chunk)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                            rusqlite::params![
                                cid, i as i64, fcid, name, ctype, size, sha, key, chunk_col
                            ],
                        )?;
                    }
                }
                tx.commit()?;
                Ok(n)
            })
            .await;
        match inserted {
            Ok(1) => {}
            Ok(_) => {
                // A first call with this id committed meanwhile: its row wins.
                self.remove_snapshots(&names).await;
                self.drop_ids(&names);
                return self.receipt_of(&client_id).await;
            }
            Err(_) => {
                self.remove_snapshots(&names).await;
                self.drop_ids(&names);
                return Err(OutboxError::Store);
            }
        }
        sender.wake.notify_one();
        self.changed(channel_id);
        self.receipt_of(&client_id).await
    }

    /// Remove snapshot files no row names (best-effort; `open` retries what's left).
    async fn remove_snapshots(&self, names: &[String]) {
        let (dir, names) = (self.snap_dir.clone(), names.to_vec());
        let _ = tokio::task::spawn_blocking(move || {
            for n in names {
                let _ = std::fs::remove_file(dir.join(n));
            }
        })
        .await;
    }

    /// How many messages haven't gone out (the sign-out warning). `accepted` ones have.
    pub(crate) async fn unsent_count(&self) -> Result<u64, StoreError> {
        self.db
            .call(|c| {
                c.query_row(
                    "SELECT count(*) FROM outbox WHERE state != 'accepted'",
                    [],
                    |r| r.get(0),
                )
            })
            .await
    }

    async fn row_channel(&self, client_id: &str) -> Result<Option<String>, OutboxError> {
        let cid = client_id.to_string();
        self.db
            .call(move |c| {
                use rusqlite::OptionalExtension;
                c.query_row(
                    "SELECT channel_id FROM outbox WHERE client_id = ?1",
                    [&cid],
                    |r| r.get(0),
                )
                .optional()
            })
            .await
            .map_err(|_| OutboxError::Store)
    }

    /// Put a failed message back in line.
    pub(crate) async fn retry(self: &Arc<Self>, client_id: &str) -> Result<(), OutboxError> {
        self.retry_row(client_id, false).await
    }

    /// Retry a failed reply as a plain message (its quote is gone: `422
    /// message.reply_target_gone`). In place, keeping its position: Delete and a new send
    /// would put it behind later messages. The same `client_id` is safe to reuse because
    /// the server stored nothing for a refused send.
    pub(crate) async fn retry_without_reply(
        self: &Arc<Self>,
        client_id: &str,
    ) -> Result<(), OutboxError> {
        self.retry_row(client_id, true).await
    }

    async fn retry_row(
        self: &Arc<Self>,
        client_id: &str,
        drop_reply: bool,
    ) -> Result<(), OutboxError> {
        self.check_open()?;
        // Stored canonical: the caller may still hold the form it passed to `enqueue`.
        let canonical = canonical_client_id(client_id);
        let client_id = canonical.as_deref().unwrap_or(client_id);
        let Some(channel) = self.row_channel(client_id).await? else {
            return Ok(());
        };
        let sender = self.sender(&channel)?;
        let _held = sender.lock.lock().await;
        // A retry is the user's word: an earlier cancel no longer holds. (Only a row that
        // has flags: none are made for a row the ack removed meanwhile.)
        if let Some(flags) = lock(&self.row_flags).get(client_id) {
            flags.cancel.store(false, Ordering::SeqCst);
            flags.pause.store(false, Ordering::SeqCst);
        }
        let cid = client_id.to_string();
        self.db
            .call(move |c| {
                c.execute(
                    "UPDATE outbox_files SET error = NULL WHERE client_id = ?1
                       AND EXISTS (SELECT 1 FROM outbox
                                   WHERE client_id = ?1 AND state = 'failed')",
                    [&cid],
                )?;
                c.execute(
                    // Without the quote only for a row refused *because* of the quote: another
                    // failure (an echo mismatch: the server did store it) keeps it as it was.
                    "UPDATE outbox SET state = 'pending', error = NULL,
                         reply_to_id = CASE WHEN ?2 THEN NULL ELSE reply_to_id END
                     WHERE client_id = ?1 AND state = 'failed'
                       AND (NOT ?2 OR error = 'message.reply_target_gone')",
                    rusqlite::params![cid, drop_reply],
                )
            })
            .await
            .map_err(|_| OutboxError::Store)?;
        sender.wake.notify_one();
        self.changed(&channel);
        Ok(())
    }

    /// Remove a message that hasn't gone out. One in flight is waited for first; one the
    /// server has (`accepted`, or taken during that attempt) is `AlreadySent`.
    pub(crate) async fn delete_pending(
        self: &Arc<Self>,
        client_id: &str,
    ) -> Result<Deleted, OutboxError> {
        self.check_open()?;
        let canonical = canonical_client_id(client_id);
        let client_id = canonical.as_deref().unwrap_or(client_id);
        let Some(channel) = self.row_channel(client_id).await? else {
            return Ok(Deleted::NotFound);
        };
        // Its uploads stop first (at the next chunk, or out of a backoff), so a Delete never
        // waits behind hours of upload for the lock. The flags are atomics behind a std
        // mutex, never held across an await: no deadlock with the sender.
        self.flags(client_id).cancel.store(true, Ordering::SeqCst);
        let sender = self.sender(&channel)?;
        let _held = sender.lock.lock().await; // an attempt in flight finishes first
        let found = self
            .remove_row(client_id)
            .await
            .map_err(|_| OutboxError::Store)?;
        sender.wake.notify_one(); // the rows behind it may go now
        self.changed(&channel);
        Ok(match found.as_deref() {
            None | Some("accepted") => Deleted::AlreadySent,
            Some(_) => Deleted::Removed,
        })
    }

    /// The channel's sender: one row at a time, by ordinal.
    async fn run_channel(self: &Arc<Self>, channel_id: &str, sender: &ChannelSender) {
        let mut backoff = Duration::from_secs(1);
        loop {
            // Paused while signed out, whoever ended the session.
            let Some(epoch) = self.signed_in().await else {
                return; // the session source is gone: the client is shutting down
            };
            let held = sender.lock.lock().await;
            let next = match self.next_row(channel_id).await {
                Ok(Some((client_id, msg, state))) if state != "failed" => {
                    (client_id, msg, state == "accepted")
                }
                // Nothing waiting, blocked by a failed row (until Retry or Delete), or the
                // store can't be read (until it comes back): wait, bounded.
                _ => {
                    drop(held);
                    let _ = tokio::time::timeout(IDLE_POLL, sender.wake.notified()).await;
                    continue;
                }
            };
            let outcome = self
                .attempt(channel_id, &next.0, &next.1, next.2, epoch)
                .await;
            drop(held);
            self.changed(channel_id); // sent, failed, accepted or back to pending
            match outcome {
                Attempt::Next => backoff = Duration::from_secs(1),
                Attempt::Wait(after) => {
                    let after = after.unwrap_or(backoff);
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    self.sleep_unless_signed_out(after, epoch).await;
                }
            }
        }
    }

    async fn next_row(
        &self,
        channel_id: &str,
    ) -> Result<Option<(String, Outgoing, String)>, StoreError> {
        let ch = channel_id.to_string();
        self.db
            .call(move |c| {
                use rusqlite::OptionalExtension;
                // Every attempt reads the whole row: a resend can't drop the reply target
                // (the server keeps the first POST per client_id and answers with it).
                c.query_row(
                    "SELECT client_id, body, reply_to_id, state FROM outbox
                     WHERE channel_id = ?1 ORDER BY ordinal LIMIT 1",
                    [&ch],
                    |r| {
                        Ok((
                            r.get(0)?,
                            Outgoing {
                                body: r.get(1)?,
                                reply_to_id: r.get(2)?,
                                attachments: vec![],
                            },
                            r.get(3)?,
                        ))
                    },
                )
                .optional()
            })
            .await
    }

    async fn set_state(
        &self,
        client_id: &str,
        state: &'static str,
        error: Option<String>,
    ) -> Result<(), StoreError> {
        let cid = client_id.to_string();
        self.db
            .call(move |c| {
                c.execute(
                    "UPDATE outbox SET state = ?2, error = ?3, attempts = attempts + (?2 = 'sending')
                     WHERE client_id = ?1",
                    rusqlite::params![cid, state, error],
                )
            })
            .await
            .map(|_| ())
    }

    fn current(&self, epoch: u64) -> bool {
        self.session.has_changed().is_ok() && *self.session.borrow() == Some(epoch)
    }

    /// One send of the channel's first row. `accepted`: the server already took it (only its
    /// acknowledgement couldn't be stored); that knowledge is never overwritten.
    async fn attempt(
        &self,
        channel_id: &str,
        client_id: &str,
        msg: &Outgoing,
        accepted: bool,
        epoch: u64,
    ) -> Attempt {
        // What a retryable outcome leaves the row as.
        let waiting = if accepted { "accepted" } else { "pending" };
        // Every outcome below is recorded before the next attempt; a store that can't
        // record it means waiting, never re-sending in a tight loop.
        if !accepted && self.set_state(client_id, "sending", None).await.is_err() {
            return Attempt::Wait(None);
        }
        self.changed(channel_id);
        // Checked right before sending, after every await, and handed to `Post`. (A POST
        // already on the wire when a sign-out lands can't be taken back; it carries this
        // user's old token, never the next session's.)
        if !self.current(epoch) {
            let _ = self.set_state(client_id, waiting, None).await;
            return Attempt::Next; // the loop pauses until the next sign-in
        }
        let Ok(files) = self.files_of(client_id).await else {
            let _ = self.set_state(client_id, waiting, None).await;
            return Attempt::Wait(None);
        };
        let mut msg = msg.clone();
        let mut reattached = false;
        // An accepted row's files are all on the server: its ids are resent as stored, and
        // no cancel or upload can touch it (the server has the message).
        if accepted {
            msg.attachments = files.iter().filter_map(|f| f.file_id.clone()).collect();
        }
        let answer = loop {
            if !files.is_empty() && !accepted {
                let files = if reattached {
                    match self.files_of(client_id).await {
                        Ok(f) => f,
                        Err(_) => {
                            let _ = self.set_state(client_id, waiting, None).await;
                            return Attempt::Wait(None);
                        }
                    }
                } else {
                    files.clone()
                };
                match self
                    .upload_files(channel_id, client_id, &files, waiting, epoch)
                    .await
                {
                    Ok(ids) => msg.attachments = ids,
                    Err(outcome) => return outcome,
                }
                // Cancelled after the last upload: the message isn't sent. (Once the POST is
                // out, a cancel is too late: the acknowledgement decides.)
                if self.flags(client_id).cancel.load(Ordering::SeqCst) {
                    return self.fail(client_id, "transfer.cancelled").await;
                }
            }
            if !self.current(epoch) {
                let _ = self.set_state(client_id, waiting, None).await;
                return Attempt::Next;
            }
            let answer = self.post.send(channel_id, &msg, client_id, epoch).await;
            if !self.current(epoch) {
                // Signed out (or someone else signed in) while it was out: not applied here.
                // The row stays; a resend is answered with the stored message.
                let _ = self.set_state(client_id, waiting, None).await;
                return Attempt::Next;
            }
            match &answer {
                // The server swept the files (unattached for 24 h) while this waited: create
                // them again by their ids (a survivor comes back as it was; a swept one gets
                // a new id and its bytes again, from the snapshot), once.
                Err(SendFailure::Refused { code })
                    if code == "file.not_attachable"
                        && !files.is_empty()
                        && !accepted
                        && !reattached =>
                {
                    reattached = true;
                    let cid = client_id.to_string();
                    if self
                        .db
                        .call(move |c| {
                            c.execute(
                                "UPDATE outbox_files SET file_id = NULL WHERE client_id = ?1",
                                [&cid],
                            )
                        })
                        .await
                        .is_err()
                    {
                        let _ = self.set_state(client_id, waiting, None).await;
                        return Attempt::Wait(None);
                    }
                }
                _ => break answer,
            }
        };
        match answer {
            Ok(message) => {
                // Only the echoed id says which row this answers. A mismatch is a bug: fail
                // the row, never resend it.
                if message.get("client_id").and_then(Value::as_str) != Some(client_id) {
                    return match self
                        .set_state(client_id, "failed", Some("outbox.echo_mismatch".into()))
                        .await
                    {
                        Ok(()) => Attempt::Next,
                        Err(_) => Attempt::Wait(None),
                    };
                }
                // 1. the message into the cache, committed; 2. only then, the row goes.
                // The second time the cache can't take it, the row goes anyway: the server
                // has the message and /sync brings it (never resent forever).
                if self.cache.apply_ack(&message).await.is_err() && !accepted {
                    let _ = self.set_state(client_id, "accepted", None).await;
                    return Attempt::Wait(None);
                }
                self.drop_row(client_id, accepted).await
            }
            // The server has an accepted message: a later refusal (removed from the channel
            // meanwhile) only means it can't be fetched back here. It stays sent.
            Err(SendFailure::Refused { .. }) if accepted => self.drop_row(client_id, true).await,
            Err(SendFailure::Refused { code }) => {
                match self.set_state(client_id, "failed", Some(code)).await {
                    Ok(()) => Attempt::Next,
                    Err(_) => Attempt::Wait(None),
                }
            }
            Err(SendFailure::Transient { retry_after }) => {
                if self.set_state(client_id, waiting, None).await.is_err() {
                    return Attempt::Wait(None);
                }
                // At least a second, whatever the server said: never a tight loop.
                Attempt::Wait(retry_after.map(|s| Duration::from_secs(s.max(1))))
            }
        }
    }

    async fn fail(&self, client_id: &str, code: &str) -> Attempt {
        match self
            .set_state(client_id, "failed", Some(code.to_string()))
            .await
        {
            Ok(()) => Attempt::Next,
            Err(_) => Attempt::Wait(None),
        }
    }

    /// Upload the row's files that aren't on the server yet, in order, each under the
    /// sender's session epoch, committing each `file_id` as it comes back. The ids of all
    /// the files, in order, or what the attempt ends with.
    async fn upload_files(
        &self,
        channel_id: &str,
        row_id: &str,
        files: &[FileRow],
        waiting: &'static str,
        epoch: u64,
    ) -> Result<Vec<String>, Attempt> {
        let flags = self.flags(row_id);
        // A pause is the session's state, and this attempt runs under a current one. A
        // cancel is the user's: only Retry clears it, so one that came just now counts.
        flags.pause.store(false, Ordering::SeqCst);
        if flags.cancel.load(Ordering::SeqCst) {
            return Err(self.fail(row_id, "transfer.cancelled").await);
        }
        // Any change away from this epoch (a sign-out, a switch, or both merged into one
        // change) pauses the uploads at their next chunk: they resume at the next sign-in.
        struct AbortOnDrop(tokio::task::JoinHandle<()>);
        impl Drop for AbortOnDrop {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        // Aborted however this ends (a return, or the sender task itself aborted by close).
        let _pauser = {
            let (flags, mut session) = (flags.clone(), self.session.clone());
            AbortOnDrop(tokio::spawn(async move {
                while *session.borrow_and_update() == Some(epoch) {
                    if session.changed().await.is_err() {
                        break;
                    }
                }
                flags.pause.store(true, Ordering::SeqCst);
            }))
        };
        self.upload_each(channel_id, row_id, files, &flags, waiting, epoch)
            .await
    }

    async fn upload_each(
        &self,
        channel_id: &str,
        row_id: &str,
        files: &[FileRow],
        flags: &Arc<Flags>,
        waiting: &'static str,
        epoch: u64,
    ) -> Result<Vec<String>, Attempt> {
        let mut ids = Vec::with_capacity(files.len());
        for f in files {
            if let Some(id) = &f.file_id {
                ids.push(id.clone());
                continue;
            }
            if !self.current(epoch) {
                let _ = self.set_state(row_id, waiting, None).await;
                return Err(Attempt::Next);
            }
            let tid = self.transfer_id(row_id, &f.file_client_id);
            let path = self.snap_dir.join(&f.file_client_id);
            let id = snapshot::id_bytes(&f.file_client_id).unwrap_or_default();
            // Verified once per process before its first byte goes out: a damaged snapshot
            // fails the row here, instead of reading as a network error mid-PUT forever.
            if !lock(&self.verified).contains(&f.file_client_id) {
                let (p, key, size, sha, chunk) =
                    (path.clone(), f.key, f.size, f.sha256.clone(), f.chunk);
                let ok = tokio::task::spawn_blocking(move || {
                    snapshot::verify(&p, &key, id, size, &sha, chunk).is_ok()
                })
                .await
                .unwrap_or(false);
                if !ok {
                    let _ = self
                        .set_file(&f.file_client_id, None, Some("outbox.snapshot_damaged"))
                        .await;
                    self.transfers.emit(
                        tid,
                        0,
                        f.size,
                        TransferState::Failed("outbox.snapshot_damaged".into()),
                    );
                    return Err(self.fail(row_id, "outbox.snapshot_damaged").await);
                }
                lock(&self.verified).insert(f.file_client_id.clone());
            }
            let source = SnapshotSource {
                path,
                key: f.key,
                id,
                size: f.size,
                sha256: f.sha256.clone(),
                chunk: f.chunk,
                broken: Arc::default(),
            };
            match self
                .upload
                .upload(tid, flags, channel_id, f, &source, epoch)
                .await
            {
                Ok(info) => {
                    if self
                        .set_file(&f.file_client_id, Some(&info.id), None)
                        .await
                        .is_err()
                    {
                        let _ = self.set_state(row_id, waiting, None).await;
                        return Err(Attempt::Wait(None));
                    }
                    self.transfers
                        .emit(tid, f.size, f.size, TransferState::Done);
                    ids.push(info.id);
                }
                Err(err) => {
                    if flags.cancel.load(Ordering::SeqCst) {
                        self.transfers
                            .emit(tid, 0, f.size, TransferState::Cancelled);
                        return Err(self.fail(row_id, "transfer.cancelled").await);
                    }
                    let code = error_code(&err);
                    if code == "transfer.io" || source.broken.load(Ordering::SeqCst) {
                        // A read error mid-PUT: verify again before the next attempt.
                        lock(&self.verified).remove(&f.file_client_id);
                    }
                    if upload_transient(&err) {
                        self.transfers.emit(
                            tid,
                            0,
                            f.size,
                            TransferState::Retrying { after_secs: 0 },
                        );
                        if self.set_state(row_id, waiting, None).await.is_err() {
                            return Err(Attempt::Wait(None));
                        }
                        // Paused by a session change: the loop waits for the next sign-in.
                        return Err(if flags.pause.load(Ordering::SeqCst) {
                            Attempt::Next
                        } else {
                            Attempt::Wait(None)
                        });
                    }
                    let _ = self.set_file(&f.file_client_id, None, Some(&code)).await;
                    self.transfers
                        .emit(tid, 0, f.size, TransferState::Failed(code.clone()));
                    return Err(self.fail(row_id, &code).await);
                }
            }
        }
        // Distinct, or the server refuses the whole send: a duplicate is a bug, never sent.
        let distinct: HashSet<&String> = ids.iter().collect();
        if distinct.len() != ids.len() {
            return Err(self.fail(row_id, "outbox.duplicate_file").await);
        }
        Ok(ids)
    }

    async fn set_file(
        &self,
        file_client_id: &str,
        file_id: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        let (f, id, e) = (
            file_client_id.to_string(),
            file_id.map(str::to_string),
            error.map(str::to_string),
        );
        self.db
            .call(move |c| {
                c.execute(
                    "UPDATE outbox_files SET file_id = COALESCE(?2, file_id), error = ?3
                     WHERE file_client_id = ?1",
                    rusqlite::params![f, id, e],
                )
            })
            .await
            .map(|_| ())
    }

    /// Remove a row with its files, in one transaction that also journals the files'
    /// snapshots (read before the cascade removes the rows naming them); the snapshots are
    /// unlinked after the commit, then the journal is cleared (`open` finishes what a crash
    /// cut short). The row's state before, if it was there.
    async fn remove_row(&self, client_id: &str) -> Result<Option<String>, StoreError> {
        let cid = client_id.to_string();
        let (state, names) = self
            .db
            .call(move |c| {
                use rusqlite::OptionalExtension;
                let tx = c.transaction()?;
                let state: Option<String> = tx
                    .query_row(
                        "SELECT state FROM outbox WHERE client_id = ?1",
                        [&cid],
                        |r| r.get(0),
                    )
                    .optional()?;
                let names: Vec<String> = tx
                    .prepare("SELECT file_client_id FROM outbox_files WHERE client_id = ?1")?
                    .query_map([&cid], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                for n in &names {
                    tx.execute("INSERT OR IGNORE INTO deletions(path) VALUES (?1)", [n])?;
                }
                tx.execute("DELETE FROM outbox WHERE client_id = ?1", [&cid])?;
                tx.commit()?;
                Ok((state, names))
            })
            .await?;
        self.forget_row(client_id, &names);
        if !names.is_empty() {
            self.remove_snapshots(&names).await;
            let done = names.clone();
            let _ = self
                .db
                .call(move |c| {
                    for n in &done {
                        c.execute("DELETE FROM deletions WHERE path = ?1", [n])?;
                    }
                    Ok(())
                })
                .await;
        }
        Ok(state)
    }

    /// The message is on the server: its row goes.
    async fn drop_row(&self, client_id: &str, accepted: bool) -> Attempt {
        match self.remove_row(client_id).await {
            Ok(_) => Attempt::Next,
            Err(_) => {
                if !accepted {
                    let _ = self.set_state(client_id, "accepted", None).await;
                }
                Attempt::Wait(None)
            }
        }
    }

    /// Waits until signed in; the current epoch. `None` if the session source is gone.
    async fn signed_in(&self) -> Option<u64> {
        let mut s = self.session.clone();
        loop {
            // The session's source is gone (the client was dropped): stop, whatever its last
            // value said. A closed channel keeps its last `Some(epoch)` forever otherwise.
            s.has_changed().ok()?;
            if let Some(epoch) = *s.borrow_and_update() {
                return Some(epoch);
            }
            s.changed().await.ok()?;
        }
    }

    async fn session_changes_from(&self, epoch: u64) {
        let mut s = self.session.clone();
        while *s.borrow_and_update() == Some(epoch) {
            if s.changed().await.is_err() {
                return;
            }
        }
    }

    /// Sleep `after`, cut short by a sign-out (the sender then pauses). Not by the channel's
    /// wake-up: a new message queues behind this one, and a stored wake-up (from its own
    /// enqueue) would cut every first backoff to nothing.
    async fn sleep_unless_signed_out(&self, after: Duration, epoch: u64) {
        tokio::select! {
            _ = tokio::time::sleep(after) => {}
            _ = self.session_changes_from(epoch) => {}
        }
    }
}

enum Attempt {
    /// Go on to the next row at once.
    Next,
    /// Wait (the server's `Retry-After`, or the backoff).
    Wait(Option<Duration>),
}

/// A failed upload's code, for the row and the file.
fn error_code(err: &crate::Error) -> String {
    match err {
        crate::Error::Api { code, .. } => code.clone(),
        crate::Error::NotAuthenticated => "auth.not_authenticated".into(),
        crate::Error::Http(_) | crate::Error::Timeout | crate::Error::Disconnected => {
            "transfer.network".into()
        }
        _ => "transfer.failed".into(),
    }
}

/// An upload failure the row waits out rather than fails on: `transfer.rs`'s transient
/// list, plus a 401 (the refresh loop renews the token, or the next sign-in resumes), an
/// unreadable answer (as for a message POST), and a local read error mid-PUT (re-verified).
fn upload_transient(err: &crate::Error) -> bool {
    crate::transfer::is_transient(err)
        || matches!(
            err,
            crate::Error::NotAuthenticated | crate::Error::UnexpectedResponse
        )
        || matches!(err, crate::Error::Api { code, .. } if code == "transfer.io")
}
