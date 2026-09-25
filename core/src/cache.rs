//! The offline cache's front (plan C3, spec §8, reading half): what the apps read, the change
//! notices they re-read on, and the triggers that keep it synced.
//!
//! Reads never touch the network. `cached_messages` says when a page needs it
//! (`needs_network`), and the app asks for it with `load_head` / `load_older`, which fetch,
//! apply and extend coverage in one step.
//!
//! Syncs are single-flight. A `sync.hint` above the cursor, and any live event that can't be
//! applied directly (`reaction.update`; anything with a `seq` but no rows), ask for one,
//! debounced: a burst of hints costs one `/sync`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use tokio::sync::{broadcast, watch, Mutex};

use crate::apply::{apply, floor_at, Applied, Batch, MessageRow};
use crate::cache_http::HISTORY_MAX;
use crate::coverage;
use crate::store::{Db, StoreError};
use crate::sync::{self, event_batch, message_row, Fetch, SyncError, Synced};

/// How long a hint (or an unappliable event) waits for others before the sync runs.
pub(crate) const HINT_DEBOUNCE: Duration = Duration::from_millis(500);

/// What changed, after it was committed. The apps re-read what they show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheEvent {
    /// Channels whose rows (the channel, its members, its messages) changed.
    Channels(Vec<String>),
    /// Channels the caller was removed from: gone from the cache.
    Removed(Vec<String>),
    /// Profiles that changed (display names, status): re-render those authors.
    Users(Vec<String>),
    /// The server reset the sync (`410`): the cache is being rebuilt.
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CacheState {
    pub syncing: bool,
    pub last_synced: Option<SystemTime>,
    /// The last sync attempt couldn't reach the server (reads still work).
    pub offline: bool,
}

/// Where history pages come from (`GET /channels/{id}/messages`).
#[async_trait::async_trait]
pub(crate) trait History: Send + Sync {
    /// Messages older than `before` (the newest page without it), newest first or in any
    /// order; at most `limit`.
    async fn page(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>, crate::Error>;
}

/// A channel as cached, with its unread count computed here (`/sync`'s is always 0).
#[derive(Debug, Clone, PartialEq)]
pub struct CachedChannel {
    pub json: Value,
    pub unread: u32,
}

/// A page of cached messages, newest first.
#[derive(Debug, Clone, PartialEq)]
pub struct MessagesPage {
    pub messages: Vec<Value>,
    /// The cache can't answer this page by itself: never opened (no coverage), or it runs
    /// below what's cached and the start isn't known. The app shows "loading" and calls
    /// `load_head` (no `before`) or `load_older`.
    pub needs_network: bool,
}

pub(crate) struct Cache {
    db: Db,
    me: String,
    fetch: Arc<dyn Fetch>,
    history: Arc<dyn History>,
    events: broadcast::Sender<CacheEvent>,
    state: watch::Sender<CacheState>,
    /// Held for a whole sync run: single-flight.
    running: Mutex<()>,
    /// A debounced sync is already scheduled.
    scheduled: std::sync::atomic::AtomicBool,
    /// Asked for while a run was going: that run may have fetched past the change, so it
    /// goes round once more.
    again: std::sync::atomic::AtomicBool,
    /// Closing: no new syncs, and the debounced ones are aborted.
    closed: std::sync::atomic::AtomicBool,
    scheduled_tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Cache {
    pub(crate) fn new(
        db: Db,
        me: String,
        fetch: Arc<dyn Fetch>,
        history: Arc<dyn History>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        let (state, _) = watch::channel(CacheState::default());
        Arc::new(Self {
            db,
            me,
            fetch,
            history,
            events,
            state,
            running: Mutex::new(()),
            scheduled: std::sync::atomic::AtomicBool::new(false),
            again: std::sync::atomic::AtomicBool::new(false),
            closed: std::sync::atomic::AtomicBool::new(false),
            scheduled_tasks: std::sync::Mutex::default(),
        })
    }

    /// Change notices. A receiver that falls behind gets `RecvError::Lagged`: it missed
    /// notices, so it re-reads everything it shows (the cache itself lost nothing).
    pub(crate) fn events(&self) -> broadcast::Receiver<CacheEvent> {
        self.events.subscribe()
    }

    pub(crate) fn state(&self) -> watch::Receiver<CacheState> {
        self.state.subscribe()
    }

    fn notify(&self, applied: Applied) {
        let sorted = |set: HashSet<String>| {
            let mut v: Vec<String> = set.into_iter().collect();
            v.sort();
            v
        };
        if !applied.removed.is_empty() {
            let _ = self
                .events
                .send(CacheEvent::Removed(sorted(applied.removed)));
        }
        if !applied.channels.is_empty() {
            let _ = self
                .events
                .send(CacheEvent::Channels(sorted(applied.channels)));
        }
        if !applied.users.is_empty() {
            let _ = self.events.send(CacheEvent::Users(sorted(applied.users)));
        }
    }

    async fn cursor(&self) -> Result<i64, StoreError> {
        self.db
            .call(|c| {
                c.query_row("SELECT cursor FROM meta WHERE id = 1", [], |r| {
                    r.get::<_, String>(0)
                })
            })
            .await
            .map(|s| s.parse().unwrap_or(0))
    }

    /// Sync until caught up. Single-flight: asked while a run is going, that run goes round
    /// once more instead (it may already have fetched past the change that asked).
    pub(crate) async fn sync_now(&self) -> Result<(), SyncError> {
        use std::sync::atomic::Ordering;
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        // The request is recorded before trying the lock: either the owner sees it (it
        // checks after every run and again after letting go), or the lock is free and this
        // caller runs it. No moment exists where neither happens.
        self.again.store(true, Ordering::SeqCst);
        loop {
            let Ok(run) = self.running.try_lock() else {
                return Ok(());
            };
            // Each run starts after the requests it clears, so it covers them.
            while self.again.swap(false, Ordering::SeqCst) {
                self.run_once().await?;
            }
            drop(run);
            if !self.again.load(Ordering::SeqCst) {
                return Ok(());
            }
        }
    }

    async fn run_once(&self) -> Result<(), SyncError> {
        /// `syncing` goes back to false however the run ends, cancellation included.
        struct Syncing<'a>(&'a watch::Sender<CacheState>);
        impl Drop for Syncing<'_> {
            fn drop(&mut self) {
                self.0.send_modify(|s| s.syncing = false);
            }
        }
        /// What committed pages changed, announced however the run ends: after an error,
        /// and also if the run is cancelled mid-way (the next run starts after them).
        struct Announce<'a>(&'a Cache, Applied);
        impl Drop for Announce<'_> {
            fn drop(&mut self) {
                self.0.notify(std::mem::take(&mut self.1));
            }
        }
        self.state.send_modify(|s| s.syncing = true);
        let _syncing = Syncing(&self.state);
        let mut applied = Announce(self, Applied::default());
        let result = sync::run_into(&self.db, &self.me, self.fetch.as_ref(), &mut applied.1).await;
        self.state.send_modify(|s| {
            // Offline means the server couldn't be reached: a refused token or a server error
            // is a different state (the app signs in again, or waits), not "offline".
            s.offline = matches!(
                result,
                Err(SyncError::Net(
                    crate::Error::Http(_) | crate::Error::Timeout | crate::Error::Disconnected
                ))
            );
            if result.is_ok() {
                s.last_synced = Some(SystemTime::now());
            }
        });
        drop(applied); // announced now, before the outcome's own events
        match result {
            Ok(Synced::Done(_)) => Ok(()),
            Ok(Synced::Reset) => {
                // The rebuild itself is C5's (it needs the store wipe); say so meanwhile.
                let _ = self.events.send(CacheEvent::Reset);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Ask for a sync in `HINT_DEBOUNCE`; asks meanwhile join it.
    pub(crate) fn schedule_sync(self: &Arc<Self>) {
        use std::sync::atomic::Ordering;
        if self.closed.load(Ordering::SeqCst) || self.scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = Arc::clone(self);
        let task = tokio::spawn(async move {
            tokio::time::sleep(HINT_DEBOUNCE).await;
            me.scheduled.store(false, Ordering::SeqCst);
            let _ = me.sync_now().await;
        });
        let mut tasks = self
            .scheduled_tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tasks.retain(|t| !t.is_finished());
        tasks.push(task);
    }

    /// Stop syncing and close the store, for a wipe or shutdown: debounced syncs are aborted,
    /// a run in progress is waited for (its page commits whole or not at all), then the
    /// database closes. Afterwards the store may be reset.
    pub(crate) async fn close(self: Arc<Self>) {
        use std::sync::atomic::Ordering;
        self.closed.store(true, Ordering::SeqCst);
        let tasks: Vec<_> = std::mem::take(
            &mut *self
                .scheduled_tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for t in &tasks {
            t.abort();
        }
        for t in tasks {
            let _ = t.await;
        }
        drop(self.running.lock().await); // a run in progress finishes first
        if let Ok(cache) = Arc::try_unwrap(self) {
            cache.db.close().await;
        }
    }

    /// `sync.hint {seq}`: nothing to do if the cache is already there.
    pub(crate) async fn hint(self: &Arc<Self>, seq: i64) {
        if self.cursor().await.is_ok_and(|c| seq <= c) {
            return;
        }
        self.schedule_sync();
    }

    /// A live WebSocket event. Rows go through the same guard as `/sync`; an event that
    /// carries a `seq` but can't be applied here (a per-viewer reaction summary) asks for a
    /// sync instead of being lost until the periodic one.
    pub(crate) async fn live_event(self: &Arc<Self>, kind: &str, data: &Value) {
        match event_batch(kind, data) {
            Some(batch) => {
                let me = self.me.clone();
                let applied = self
                    .db
                    .call(move |c| {
                        let tx = c.transaction()?;
                        let applied = apply(&tx, &me, &batch)?;
                        tx.commit()?;
                        Ok(applied)
                    })
                    .await;
                if let Ok(applied) = applied {
                    self.notify(applied);
                }
            }
            None => {
                if let Some(seq) = data.get("seq").and_then(Value::as_i64) {
                    self.hint(seq).await;
                }
            }
        }
    }

    /// A send's acknowledgement (the stored message the server returned): through the same
    /// guard as everything else (a later edit or tombstone already cached wins), committed
    /// before this returns, so the outbox may then drop its row (spec §5.3).
    pub(crate) async fn apply_ack(&self, message: &Value) -> Result<(), StoreError> {
        let Some(row) = message_row(message) else {
            return Err(StoreError::Sql);
        };
        let me = self.me.clone();
        let applied = self
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let applied = apply(
                    &tx,
                    &me,
                    &Batch {
                        messages: vec![row],
                        ..Batch::default()
                    },
                )?;
                tx.commit()?;
                Ok(applied)
            })
            .await?;
        self.notify(applied);
        Ok(())
    }

    /// The channels the caller is in, with unread counts.
    pub(crate) async fn cached_channels(&self) -> Result<Vec<CachedChannel>, StoreError> {
        let me = self.me.clone();
        self.db
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT ch.json,
                       (SELECT count(*) FROM messages m
                        WHERE m.channel_id = ch.id
                          AND m.id > coalesce(json_extract(my.json, '$.last_read_message_id'), '')
                          AND coalesce(json_extract(m.json, '$.author_id'), '') != ?1
                          AND json_extract(m.json, '$.deleted') IS NOT 1
                          AND json_type(m.json, '$.deleted_at') IS NOT 'text')
                     FROM channels ch
                     JOIN memberships my ON my.channel_id = ch.id AND my.user_id = ?1 AND my.left = 0
                     ORDER BY ch.id",
                )?;
                let rows = stmt.query_map([&me], |r| {
                    let json: String = r.get(0)?;
                    Ok((json, r.get::<_, u32>(1)?))
                })?;
                rows.map(|row| {
                    let (json, unread) = row?;
                    Ok(CachedChannel {
                        json: serde_json::from_str(&json).unwrap_or(Value::Null),
                        unread,
                    })
                })
                .collect()
            })
            .await
    }

    /// A page of cached messages, newest first: before `before` if given. Never the network.
    pub(crate) async fn cached_messages(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<MessagesPage, StoreError> {
        let (channel_id, before) = (channel_id.to_string(), before.map(str::to_string));
        self.db
            .call(move |c| {
                let tx = c.transaction()?;
                let range = coverage::range(&tx, &channel_id)?;
                let mut stmt = tx.prepare(
                    "SELECT json FROM messages
                     WHERE channel_id = ?1 AND (?2 IS NULL OR id < ?2)
                     ORDER BY id DESC LIMIT ?3",
                )?;
                let messages: Vec<Value> = stmt
                    .query_map(rusqlite::params![channel_id, before, limit as i64], |r| {
                        r.get::<_, String>(0)
                    })?
                    .map(|j| j.map(|j| serde_json::from_str(&j).unwrap_or(Value::Null)))
                    .collect::<rusqlite::Result<_>>()?;
                drop(stmt);
                let id_of = |m: &Value| m.get("id").and_then(Value::as_str).map(str::to_string);
                let needs_network = match &range {
                    None => true, // never opened: fetch the head
                    Some(r) => {
                        // Nothing above the range's top: a live message there may follow one
                        // not delivered yet (the next sync, or `load_head`, settles it).
                        // Measured at the page's upper edge: `before` when paging, else the
                        // first message that isn't a tombstone (the head fetch, which omits
                        // deleted messages, proved nothing live sits above the range).
                        let deleted = |m: &&Value| {
                            m.get("deleted").and_then(Value::as_bool) == Some(true)
                                || m.get("deleted_at").is_some_and(|d| !d.is_null())
                        };
                        let edge = match &before {
                            Some(b) => Some(b.clone()),
                            None => messages.iter().find(|m| !deleted(m)).and_then(id_of),
                        };
                        let top_ok = edge.is_none_or(|edge| {
                            r.newest_id
                                .as_deref()
                                .is_some_and(|top| edge.as_str() <= top)
                        });
                        // Down to the start, or a full page that ends inside the range.
                        let bottom_ok = r.complete_to_start
                            || (messages.len() >= limit
                                && messages
                                    .last()
                                    .and_then(id_of)
                                    .zip(r.oldest_id.clone())
                                    .is_some_and(|(last, oldest)| last >= oldest));
                        !(top_ok && bottom_ok)
                    }
                };
                Ok(MessagesPage {
                    messages,
                    needs_network,
                })
            })
            .await
    }

    /// Fetch the newest page and make it the channel's covered range.
    pub(crate) async fn load_head(
        &self,
        channel_id: &str,
        limit: usize,
    ) -> Result<(), crate::Error> {
        self.load(channel_id, None, limit).await
    }

    /// Fetch the page below the covered range and extend it down.
    pub(crate) async fn load_older(
        &self,
        channel_id: &str,
        limit: usize,
    ) -> Result<(), crate::Error> {
        let id = channel_id.to_string();
        let oldest = self
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                Ok(coverage::range(&tx, &id)?.and_then(|r| r.oldest_id))
            })
            .await
            .map_err(|_| crate::Error::UnexpectedResponse)?;
        match oldest {
            Some(oldest) => self.load(channel_id, Some(oldest), limit).await,
            None => self.load(channel_id, None, limit).await,
        }
    }

    async fn load(
        &self,
        channel_id: &str,
        before: Option<String>,
        limit: usize,
    ) -> Result<(), crate::Error> {
        // The channel's removal floor as of now: a removal while this request is out makes
        // its answer stale, and `apply` drops it (`Batch::history_floors`).
        let id = channel_id.to_string();
        let floor = self
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                floor_at(&tx, &id)
            })
            .await
            .map_err(|_| crate::Error::UnexpectedResponse)?;
        // One limit for the request and for coverage: the server caps a page at 100, and a
        // full capped page must never read as "short, so that was the start".
        let limit = limit.clamp(1, HISTORY_MAX);
        let rows = self
            .history
            .page(channel_id, before.as_deref(), limit)
            .await?;
        let messages: Vec<MessageRow> = rows.iter().filter_map(message_row).collect();
        let ids: Vec<String> = messages.iter().map(|m| m.id.clone()).collect();
        let (me, id) = (self.me.clone(), channel_id.to_string());
        let applied = self
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                // Coverage only from a page that could land: no removal since the request
                // started, and the channel is here (history for a channel not yet synced
                // is dropped by `apply`, so it proves nothing).
                let unchanged_floor = floor_at(&tx, &id)? == floor
                    && tx
                        .query_row("SELECT 1 FROM channels WHERE id = ?1", [&id], |_| Ok(()))
                        .is_ok()
                    // An active removal fence rejects the page's rows (the caller isn't back
                    // yet, whatever a channel row says).
                    && tx
                        .query_row(
                            "SELECT 1 FROM removed WHERE channel_id = ?1 AND active = 1",
                            [&id],
                            |_| Ok(()),
                        )
                        .is_err();
                let applied = apply(
                    &tx,
                    &me,
                    &Batch {
                        messages,
                        history: true,
                        history_floors: [(id.clone(), floor)].into(),
                        ..Batch::default()
                    },
                )?;
                // Coverage only from a page that landed.
                if unchanged_floor {
                    match &before {
                        None => coverage::record_head(&tx, &id, &ids, limit)?,
                        Some(b) => coverage::record_older(&tx, &id, b, &ids, limit)?,
                    }
                }
                tx.commit()?;
                Ok(applied)
            })
            .await
            .map_err(|_| crate::Error::UnexpectedResponse)?;
        let mut applied = applied;
        applied.channels.insert(channel_id.to_string()); // coverage changed, even if no rows did
        self.notify(applied);
        Ok(())
    }
}
