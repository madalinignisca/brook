//! The outbox (plan C4, #63; spec §5): messages written while offline (or before the server
//! answered) are durable, go out in the order written, once, and are never dropped
//! silently.
//!
//! - **Queued means durable:** `enqueue` commits the row before it returns.
//! - **One sender per channel, by ordinal.** A failed row blocks only its own channel.
//! - **Ack order:** the returned message is applied to the cache and committed first, then
//!   the row whose `client_id` equals the *echoed* one is deleted. A crash in between is a
//!   resend with the same `client_id`, which the server answers with the stored message.
//! - **Status table:** unreachable, 5xx, 408 and 429 stay pending (retried after
//!   `Retry-After` or a backoff); 401 waits for the session to be renewed; any other refusal
//!   fails the row with the server's code (Retry and Delete).
//! - **Signed out means paused**, however the session ended; each attempt checks that the
//!   session it started under is still the one signed in.
//! - Retry and Delete go through the channel sender's lock: a Delete of a row in flight
//!   waits for that attempt, and a send the server accepted can't be taken back.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{watch, Mutex, Notify};

use crate::cache::Cache;
use crate::store::{Db, StoreError};

/// Where a sent message goes (`POST /channels/{id}/messages`).
#[async_trait::async_trait]
pub(crate) trait Post: Send + Sync {
    async fn send(
        &self,
        channel_id: &str,
        body: &str,
        client_id: &str,
    ) -> Result<Value, SendFailure>;
}

/// How a send attempt failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendFailure {
    /// Try again later: unreachable, 5xx, 408, 429 (`retry_after` from the server, if any).
    Transient { retry_after: Option<u64> },
    /// The token was refused: wait for the session to be renewed, then try again.
    Unauthorized,
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
    Failed { code: String },
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
    /// Removed before it was sent.
    Removed,
    /// The server had already accepted it: it's a sent message now (sends can't be revoked).
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
    #[error("local storage failed")]
    Store,
    #[error("the server refused the message ({0})")]
    Refused(String),
}

/// Transient failures back off up to this.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

pub(crate) struct Outbox {
    db: Db,
    cache: Arc<Cache>,
    post: Arc<dyn Post>,
    session: Session,
    channels: StdMutex<HashMap<String, Arc<ChannelSender>>>,
    tasks: StdMutex<Vec<tokio::task::JoinHandle<()>>>,
}

/// One channel's sender: its lock (held for a whole attempt, and by Retry and Delete) and
/// its wake-up.
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
        }))
    }

    fn sender(self: &Arc<Self>, channel_id: &str) -> Arc<ChannelSender> {
        let mut channels = self
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(s) = channels.get(channel_id) {
            return s.clone();
        }
        let s = Arc::new(ChannelSender {
            lock: Mutex::new(()),
            wake: Notify::new(),
        });
        channels.insert(channel_id.to_string(), s.clone());
        let (me, id, sender) = (Arc::clone(self), channel_id.to_string(), s.clone());
        let task = tokio::spawn(async move { me.run_channel(&id, &sender).await });
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task);
        s
    }

    /// Stop every sender and close the store (sign-out wipes, shutdown). A sender stopped
    /// mid-send is like a crash: its row stays `sending`, and the next `open` resends it
    /// with the same `client_id`.
    pub(crate) async fn close(self: Arc<Self>) {
        let tasks: Vec<_> = std::mem::take(
            &mut *self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for t in &tasks {
            t.abort();
        }
        for t in tasks {
            let _ = t.await;
        }
        if let Ok(outbox) = Arc::try_unwrap(self) {
            outbox.db.close().await;
        }
    }

    /// Start the senders of every channel with rows waiting (at startup, after `open`).
    pub(crate) async fn resume(self: &Arc<Self>) -> Result<(), StoreError> {
        let channels: Vec<String> = self
            .db
            .call(|c| {
                c.prepare("SELECT DISTINCT channel_id FROM outbox")?
                    .query_map([], |r| r.get(0))?
                    .collect()
            })
            .await?;
        for id in channels {
            self.sender(&id).wake.notify_one();
        }
        Ok(())
    }

    /// Queue a message: durable (committed) before this returns. The channel's sender takes
    /// it from there. If the outbox can't be written, it is sent directly, but only where
    /// nothing is waiting (it would overtake them otherwise).
    pub(crate) async fn enqueue(
        self: &Arc<Self>,
        channel_id: &str,
        body: &str,
    ) -> Result<String, OutboxError> {
        let client_id = new_client_id();
        let (ch, b, cid) = (channel_id.to_string(), body.to_string(), client_id.clone());
        let queued = self
            .db
            .call(move |c| {
                c.execute(
                    "INSERT INTO outbox(client_id, channel_id, body, state, created_at)
                     VALUES (?1, ?2, ?3, 'pending', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
                    [&cid, &ch, &b],
                )
            })
            .await;
        match queued {
            Ok(_) => {
                self.sender(channel_id).wake.notify_one();
                Ok(client_id)
            }
            Err(_) => self.send_direct(channel_id, body, client_id).await,
        }
    }

    /// The outbox couldn't take the row (disk full): send now, not queued, only if this
    /// channel has nothing waiting.
    async fn send_direct(
        &self,
        channel_id: &str,
        body: &str,
        client_id: String,
    ) -> Result<String, OutboxError> {
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
        match self.post.send(channel_id, body, &client_id).await {
            Ok(message) => {
                let _ = self.cache.apply_ack(&message).await;
                Ok(client_id)
            }
            Err(SendFailure::Refused { code }) => Err(OutboxError::Refused(code)),
            Err(_) => Err(OutboxError::Refused("network".into())),
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

    /// How many messages haven't gone out (the sign-out warning).
    pub(crate) async fn unsent_count(&self) -> Result<u64, StoreError> {
        self.db
            .call(|c| c.query_row("SELECT count(*) FROM outbox", [], |r| r.get(0)))
            .await
    }

    async fn row_channel(&self, client_id: &str) -> Result<Option<String>, StoreError> {
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
    }

    /// Put a failed message back in line.
    pub(crate) async fn retry(self: &Arc<Self>, client_id: &str) -> Result<(), StoreError> {
        let Some(channel) = self.row_channel(client_id).await? else {
            return Ok(());
        };
        let sender = self.sender(&channel);
        let _held = sender.lock.lock().await;
        let cid = client_id.to_string();
        self.db
            .call(move |c| {
                c.execute(
                    "UPDATE outbox SET state = 'pending', error = NULL WHERE client_id = ?1 AND state = 'failed'",
                    [&cid],
                )
            })
            .await?;
        sender.wake.notify_one();
        Ok(())
    }

    /// Remove a message that hasn't gone out. One in flight is waited for first: if the
    /// server accepted it, it's sent (and stays).
    pub(crate) async fn delete_pending(
        self: &Arc<Self>,
        client_id: &str,
    ) -> Result<Deleted, StoreError> {
        let Some(channel) = self.row_channel(client_id).await? else {
            return Ok(Deleted::NotFound);
        };
        let sender = self.sender(&channel);
        let _held = sender.lock.lock().await; // an attempt in flight finishes first
        let cid = client_id.to_string();
        let removed = self
            .db
            .call(move |c| c.execute("DELETE FROM outbox WHERE client_id = ?1", [&cid]))
            .await?;
        sender.wake.notify_one(); // the rows behind it may go now
        Ok(if removed > 0 {
            Deleted::Removed
        } else {
            Deleted::AlreadySent
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
                Ok(next) => next,
                Err(_) => {
                    drop(held);
                    sender.wake.notified().await;
                    continue;
                }
            };
            let Some((client_id, body, failed)) = next else {
                drop(held);
                sender.wake.notified().await; // nothing waiting
                continue;
            };
            if failed {
                drop(held);
                sender.wake.notified().await; // blocked until Retry or Delete
                continue;
            }
            let wait = self.attempt(channel_id, &client_id, &body, epoch).await;
            drop(held);
            match wait {
                Attempt::Next => backoff = Duration::from_secs(1),
                Attempt::Wait(after) => {
                    let after = after.unwrap_or(backoff);
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    self.sleep_unless_signed_out(after, epoch).await;
                }
                Attempt::WaitForSession => {
                    self.session_changes_from(epoch).await;
                }
            }
        }
    }

    async fn next_row(
        &self,
        channel_id: &str,
    ) -> Result<Option<(String, String, bool)>, StoreError> {
        let ch = channel_id.to_string();
        self.db
            .call(move |c| {
                use rusqlite::OptionalExtension;
                c.query_row(
                    "SELECT client_id, body, state = 'failed' FROM outbox
                     WHERE channel_id = ?1 ORDER BY ordinal LIMIT 1",
                    [&ch],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
            })
            .await
    }

    async fn set_state(&self, client_id: &str, state: &'static str, error: Option<String>) {
        let cid = client_id.to_string();
        let _ = self
            .db
            .call(move |c| {
                c.execute(
                    "UPDATE outbox SET state = ?2, error = ?3, attempts = attempts + (?2 = 'sending')
                     WHERE client_id = ?1",
                    rusqlite::params![cid, state, error],
                )
            })
            .await;
    }

    async fn attempt(&self, channel_id: &str, client_id: &str, body: &str, epoch: u64) -> Attempt {
        // Signed out (or someone else signed in) since this row was picked: not under this
        // session. The loop pauses until the next sign-in.
        if *self.session.borrow() != Some(epoch) {
            return Attempt::Next;
        }
        self.set_state(client_id, "sending", None).await;
        let answer = self.post.send(channel_id, body, client_id).await;
        match answer {
            Ok(message) => {
                // Only the echoed id says which row this answers. A mismatch is a bug: fail
                // the row, never resend it.
                if message.get("client_id").and_then(Value::as_str) != Some(client_id) {
                    self.set_state(client_id, "failed", Some("outbox.echo_mismatch".into()))
                        .await;
                    return Attempt::Next;
                }
                // 1. the message into the cache, committed; 2. only then, the row goes.
                if self.cache.apply_ack(&message).await.is_err() {
                    self.set_state(client_id, "pending", None).await;
                    return Attempt::Wait(None);
                }
                let cid = client_id.to_string();
                let _ = self
                    .db
                    .call(move |c| c.execute("DELETE FROM outbox WHERE client_id = ?1", [&cid]))
                    .await;
                Attempt::Next
            }
            Err(SendFailure::Refused { code }) => {
                self.set_state(client_id, "failed", Some(code)).await;
                Attempt::Next
            }
            Err(SendFailure::Transient { retry_after }) => {
                self.set_state(client_id, "pending", None).await;
                Attempt::Wait(retry_after.map(Duration::from_secs))
            }
            Err(SendFailure::Unauthorized) => {
                self.set_state(client_id, "pending", None).await;
                // The session may already have moved on (renewed, or signed out).
                if *self.session.borrow() == Some(epoch) {
                    Attempt::WaitForSession
                } else {
                    Attempt::Next
                }
            }
        }
    }

    /// Waits until signed in; the current epoch. `None` if the session source is gone.
    async fn signed_in(&self) -> Option<u64> {
        let mut s = self.session.clone();
        loop {
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
    /// Transient: wait (the server's `Retry-After`, or the backoff).
    Wait(Option<Duration>),
    /// 401: wait until the session changes (renewed or ended).
    WaitForSession,
}
