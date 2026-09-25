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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{watch, Mutex, Notify};

use crate::cache::Cache;
use crate::store::{Db, StoreError};

/// Where a sent message goes (`POST /channels/{id}/messages`). `epoch` is the session the
/// sender checked: an implementation refuses to send (`Transient`) if the signed-in session
/// is no longer that one, so a message never goes out under another session.
#[async_trait::async_trait]
pub(crate) trait Post: Send + Sync {
    async fn send(
        &self,
        channel_id: &str,
        body: &str,
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
    pub(crate) async fn open(
        db: Db,
        cache: Arc<Cache>,
        post: Arc<dyn Post>,
        session: Session,
    ) -> Result<Arc<Self>, StoreError> {
        db.call(|c| {
            c.execute(
                "UPDATE outbox SET state = 'pending' WHERE state = 'sending'",
                [],
            )
        })
        .await?;
        Ok(Arc::new(Self {
            db,
            cache,
            post,
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
    /// earlier attempt to retry it (never a second message). If the outbox can't be
    /// written, it's sent directly, only where nothing is waiting.
    pub(crate) async fn enqueue(
        self: &Arc<Self>,
        channel_id: &str,
        body: &str,
        client_id: Option<String>,
    ) -> Result<String, OutboxError> {
        let sender = self.sender(channel_id)?;
        let client_id = client_id.unwrap_or_else(new_client_id);
        // Woken before the insert, not after: a caller cancelled mid-insert leaves a
        // committed row, and the sender must still look (a spare wake-up is harmless).
        sender.wake.notify_one();
        let (ch, b, cid) = (channel_id.to_string(), body.to_string(), client_id.clone());
        let queued = self
            .db
            .call(move |c| {
                c.execute(
                    "INSERT INTO outbox(client_id, channel_id, body, state, created_at)
                     VALUES (?1, ?2, ?3, 'pending', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
                     ON CONFLICT(client_id) DO NOTHING",
                    [&cid, &ch, &b],
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
            Err(_) => self.send_direct(&sender, channel_id, body, client_id).await,
        }
    }

    /// The outbox couldn't take the row (disk full): send now, not queued, only if this
    /// channel has nothing waiting, and only while signed in. Under the channel's lock, so
    /// neither another direct send nor the queue can overtake it.
    async fn send_direct(
        &self,
        sender: &ChannelSender,
        channel_id: &str,
        body: &str,
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
        match self.post.send(channel_id, body, &client_id, epoch).await {
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

    /// The channel's waiting messages, in the order they will go.
    pub(crate) async fn pending(
        &self,
        channel_id: &str,
    ) -> Result<Vec<PendingMessage>, StoreError> {
        let ch = channel_id.to_string();
        self.db
            .call(move |c| {
                c.prepare(
                    "SELECT client_id, channel_id, body, state, error FROM outbox
                     WHERE channel_id = ?1 ORDER BY ordinal",
                )?
                .query_map([&ch], |r| {
                    let state: String = r.get(3)?;
                    Ok(PendingMessage {
                        client_id: r.get(0)?,
                        channel_id: r.get(1)?,
                        body: r.get(2)?,
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
                .collect()
            })
            .await
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
        self.check_open()?;
        let Some(channel) = self.row_channel(client_id).await? else {
            return Ok(());
        };
        let sender = self.sender(&channel)?;
        let _held = sender.lock.lock().await;
        let cid = client_id.to_string();
        self.db
            .call(move |c| {
                c.execute(
                    "UPDATE outbox SET state = 'pending', error = NULL
                     WHERE client_id = ?1 AND state = 'failed'",
                    [&cid],
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
        let Some(channel) = self.row_channel(client_id).await? else {
            return Ok(Deleted::NotFound);
        };
        let sender = self.sender(&channel)?;
        let _held = sender.lock.lock().await; // an attempt in flight finishes first
        let cid = client_id.to_string();
        let found: Option<String> = self
            .db
            .call(move |c| {
                use rusqlite::OptionalExtension;
                let state: Option<String> = c
                    .query_row(
                        "SELECT state FROM outbox WHERE client_id = ?1",
                        [&cid],
                        |r| r.get(0),
                    )
                    .optional()?;
                c.execute("DELETE FROM outbox WHERE client_id = ?1", [&cid])?;
                Ok(state)
            })
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
                Ok(Some((client_id, body, state))) if state != "failed" => {
                    (client_id, body, state == "accepted")
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
    ) -> Result<Option<(String, String, String)>, StoreError> {
        let ch = channel_id.to_string();
        self.db
            .call(move |c| {
                use rusqlite::OptionalExtension;
                c.query_row(
                    "SELECT client_id, body, state FROM outbox
                     WHERE channel_id = ?1 ORDER BY ordinal LIMIT 1",
                    [&ch],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
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
        body: &str,
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
        let answer = self.post.send(channel_id, body, client_id, epoch).await;
        if !self.current(epoch) {
            // Signed out (or someone else signed in) while it was out: not applied here.
            // The row stays; a resend is answered with the stored message.
            let _ = self.set_state(client_id, waiting, None).await;
            return Attempt::Next;
        }
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

    /// The message is on the server: its row goes.
    async fn drop_row(&self, client_id: &str, accepted: bool) -> Attempt {
        let cid = client_id.to_string();
        match self
            .db
            .call(move |c| c.execute("DELETE FROM outbox WHERE client_id = ?1", [&cid]))
            .await
        {
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
