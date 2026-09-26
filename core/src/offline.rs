//! The offline cache and outbox, per signed-in user (plan C5): opened when a user signs in,
//! fed by the WebSocket, paused when signed out, wiped on request.
//!
//! - **Signed in as U** (any path: login, TOTP, restore): U's stores open (another user's
//!   close first) and a pump starts: raw WebSocket events go through the cache's guard,
//!   `ready` (every (re)connect) and a periodic tick run a `/sync`, `sync.hint` asks for one.
//!   The outbox follows the session: paused while signed out, and never sending under
//!   another session.
//! - **Sign out, keeping the data:** the stores stay (reads work offline; unsent messages
//!   wait for the next sign-in of that user).
//! - **Sign out with "Remove this device's data"** (`forget`): the stores close and are
//!   erased **locally, first**, whatever the network does after.
//! - **Another user signed in before** (#46 §8): their stores are listed with their unsent
//!   count, for the app to say so, and wiped on `wipe_others`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{broadcast, watch};

use crate::cache::{Cache, CacheEvent, CacheState, History};
use crate::local::LocalData;
use crate::outbox::{Outbox, Post};
use crate::store::{Opened, StoreError};
use crate::sync::Fetch;

/// A periodic `/sync` catches anything a dropped event missed (spec §4.2).
pub(crate) const PERIODIC_SYNC: Duration = Duration::from_secs(300);

/// The network side a user's cache and outbox use.
#[derive(Clone)]
pub(crate) struct Net {
    pub(crate) fetch: Arc<dyn Fetch>,
    pub(crate) history: Arc<dyn History>,
    pub(crate) post: Arc<dyn Post>,
    pub(crate) upload: Arc<dyn crate::outbox::Upload>,
    pub(crate) download: Arc<dyn crate::files::Download>,
    pub(crate) transfers: Arc<crate::transfer::Transfers>,
}

/// Lost unsent messages: `newest` numbers every loss (it never restarts), `unacked` holds
/// until the app acknowledges that number.
#[derive(Debug, Default)]
pub(crate) struct Losses {
    newest: u64,
    unacked: bool,
}

impl Losses {
    /// The newest loss the app hasn't acknowledged.
    pub(crate) fn current(&self) -> Option<u64> {
        self.unacked.then_some(self.newest)
    }

    /// Clears only if `n` is still the newest: a loss after the app read `n` stays.
    pub(crate) fn acknowledge(&mut self, n: u64) {
        if n == self.newest {
            self.unacked = false;
        }
    }
}

/// Record a loss, then say so (the notice is a hint to re-read `current`).
pub(crate) fn record_loss(losses: &Mutex<Losses>, events: &broadcast::Sender<CacheEvent>) {
    {
        let mut l = losses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        l.newest += 1;
        l.unacked = true;
    }
    let _ = events.send(CacheEvent::OutboxLost);
}

/// Aborts its task when dropped, however the owner ends.
pub(crate) struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One user's open stores and the tasks feeding them.
pub(crate) struct Active {
    pub(crate) origin: String,
    pub(crate) user_id: String,
    pub(crate) cache: Arc<Cache>,
    pub(crate) outbox: Arc<Outbox>,
    pub(crate) files: Arc<crate::files::Files>,
    session: watch::Sender<Option<u64>>,
    /// The session epoch the state feed follows: a new one restarts it.
    epoch: u64,
    pump: tokio::task::JoinHandle<()>,
    /// Copies the cache's state to the client's feed while this user is signed in.
    state_feed: Option<AbortOnDrop>,
}

/// The client-level state feed: per-user forwarders write it only while their generation
/// is current, checked inside the watch's lock, so a reset (generation bumped, then
/// default sent through the same lock) is never overwritten by the previous user's cache.
#[derive(Clone)]
pub(crate) struct StateFeed {
    out: watch::Sender<CacheState>,
    generation: Arc<AtomicU64>,
}

impl StateFeed {
    pub(crate) fn new(out: watch::Sender<CacheState>) -> Self {
        Self {
            out,
            generation: Arc::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn subscribe(&self) -> watch::Receiver<CacheState> {
        self.out.subscribe()
    }

    /// Back to the default state; forwarders started before this write nothing more.
    pub(crate) fn reset(&self) -> u64 {
        let mine = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.out.send_replace(CacheState::default());
        mine
    }

    /// Follow `cache`'s state from now on (after a reset).
    pub(crate) fn follow(&self, cache: &Arc<Cache>) -> AbortOnDrop {
        let mine = self.reset();
        let (out, generation) = (self.out.clone(), self.generation.clone());
        let mut rx = cache.state();
        AbortOnDrop(tokio::spawn(async move {
            loop {
                let now = rx.borrow_and_update().clone();
                out.send_if_modified(|s| {
                    if generation.load(Ordering::SeqCst) != mine || *s == now {
                        return false;
                    }
                    *s = now;
                    true
                });
                if rx.changed().await.is_err() {
                    return;
                }
            }
        }))
    }
}

pub(crate) struct Offline {
    local: LocalData,
    active: Option<Active>,
    /// One notice stream for the app, whoever is signed in.
    events: broadcast::Sender<CacheEvent>,
    state: StateFeed,
    losses: Arc<Mutex<Losses>>,
    /// The last "Remove this device's data": that user at that session epoch or earlier
    /// never gets stores again (a sign-in event still queued from before the wipe would
    /// otherwise recreate them). The next sign-in is a later epoch.
    forgotten: Option<(String, String, u64)>,
}

impl Offline {
    #[cfg(test)]
    pub(crate) fn new(local: LocalData) -> Self {
        Self::with_events(
            local,
            broadcast::channel(512).0,
            StateFeed::new(watch::channel(CacheState::default()).0),
            Arc::default(),
        )
    }

    /// Notices go to `events`, the state to `state` and losses to `losses`: the client's,
    /// which outlive any one user's stores.
    pub(crate) fn with_events(
        local: LocalData,
        events: broadcast::Sender<CacheEvent>,
        state: StateFeed,
        losses: Arc<Mutex<Losses>>,
    ) -> Self {
        Self {
            local,
            active: None,
            events,
            state,
            losses,
            forgotten: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn losses(&self) -> Option<u64> {
        self.losses.lock().unwrap().current()
    }

    #[cfg(test)]
    pub(crate) fn state_feed(&self) -> watch::Receiver<CacheState> {
        self.state.out.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn events(&self) -> broadcast::Receiver<CacheEvent> {
        self.events.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn active(&self) -> Option<&Active> {
        self.active.as_ref()
    }

    /// The open stores, if they are `(origin, user_id)`'s: callers pass the session's
    /// user, so one user never reads or queues into another's stores while the switch is
    /// still on its way.
    pub(crate) fn active_for(&self, origin: &str, user_id: &str) -> Option<&Active> {
        self.active
            .as_ref()
            .filter(|a| a.origin == origin && a.user_id == user_id)
    }

    /// `user_id` at `origin` is signed in (session `epoch`). Opens their stores if they
    /// aren't the ones open; otherwise just resumes the outbox under the new session.
    /// `None` when their stores are locked or damaged: that user runs online-only.
    pub(crate) async fn signed_in(
        &mut self,
        origin: &str,
        user_id: &str,
        epoch: u64,
        net: Net,
        raw: broadcast::Receiver<(String, Value)>,
    ) -> Result<bool, StoreError> {
        if let Some((o, u, e)) = &self.forgotten {
            if o == origin && u == user_id && epoch <= *e {
                return Ok(false);
            }
        }
        if let Some(a) = &mut self.active {
            if a.origin == origin && a.user_id == user_id {
                // A new epoch restarts the feed: a sign-out and sign-in of this user can
                // reach the watcher as one change, with no `signed_out` in between.
                if a.epoch != epoch {
                    a.epoch = epoch;
                    a.state_feed = Some(self.state.follow(&a.cache));
                }
                a.session.send_replace(Some(epoch));
                return Ok(true);
            }
        }
        self.close_active().await;
        let opened = self.local.open_user(origin, user_id).await;
        // Whether or not the open then succeeded: a loss is flagged before the rebuild
        // that causes it.
        if self.local.take_lost_unsent() {
            record_loss(&self.losses, &self.events);
        }
        let stores = opened?;
        let store_dir = stores.dir.clone();
        let (cache_db, outbox_db) = match (stores.cache, stores.outbox) {
            (Opened::Ready { db: c, .. }, Opened::Ready { db: o, .. }) => (c, o),
            (c, o) => {
                // Either store locked or damaged: online-only for this user, nothing deleted.
                if let Opened::Ready { db, .. } = c {
                    db.close().await;
                }
                if let Opened::Ready { db, .. } = o {
                    db.close().await;
                }
                return Ok(false);
            }
        };
        let cache = Cache::new(cache_db, user_id.to_string(), net.fetch, net.history);
        let (session, session_rx) = watch::channel(Some(epoch));
        let files = crate::files::Files::open(
            cache.clone(),
            &store_dir,
            crate::files::open_dir_for(&stores.store_id),
            net.download,
            net.transfers.clone(),
            session_rx.clone(),
        )
        .await;
        let outbox = Outbox::open(
            outbox_db,
            cache.clone(),
            net.post,
            net.upload,
            net.transfers,
            session_rx,
            &store_dir,
        )
        .await?;
        outbox.set_events(self.events.clone());
        let _ = outbox.resume().await;
        let pump = tokio::spawn(pump(cache.clone(), self.events.clone(), raw));
        let state_feed = Some(self.state.follow(&cache));
        self.active = Some(Active {
            origin: origin.to_string(),
            user_id: user_id.to_string(),
            cache,
            outbox,
            files,
            session,
            epoch,
            pump,
            state_feed,
        });
        Ok(true)
    }

    /// Signed out, keeping the data: the outbox pauses, the stores stay open for reads,
    /// and the state feed goes back to default until this user signs in again.
    pub(crate) fn signed_out(&mut self) {
        if let Some(a) = &mut self.active {
            a.session.send_replace(None);
            a.state_feed = None;
            a.files.clear_open_copies(); // no plaintext copy outlives the session
        }
        self.state.reset();
    }

    /// Close the open stores (a user switch, a wipe, or the client going away).
    pub(crate) async fn close_active(&mut self) {
        self.state.reset();
        if let Some(a) = self.active.take() {
            a.session.send_replace(None);
            drop(a.state_feed);
            a.pump.abort();
            let _ = a.pump.await;
            // Downloads first, joined: nothing writes into the store once it closes (a wipe
            // erases right after this). Open copies go too.
            a.files.close().await;
            a.files.clear_open_copies();
            a.outbox.close().await;
            a.cache.close().await;
        }
    }

    /// The client is going away: close the open stores and the index, threads joined.
    pub(crate) async fn close(mut self) {
        self.close_active().await;
        self.local.close().await;
    }

    /// "Remove this device's data" for `(origin, user_id)`, signed in at `epoch`: close
    /// their stores if open and erase them, locally and first (the caller signs out of the
    /// server afterwards, whether or not that works). Stores that never opened (locked,
    /// damaged) are erased too: nothing is read to erase them.
    pub(crate) async fn forget(
        &mut self,
        origin: &str,
        user_id: &str,
        epoch: u64,
    ) -> Result<(), StoreError> {
        self.forgotten = Some((origin.to_string(), user_id.to_string(), epoch));
        // Whoever's stores are open, the feed stops showing them now: the session has moved
        // on (a switch the watcher hasn't reached yet leaves another user's cache open).
        if let Some(a) = &mut self.active {
            a.state_feed = None;
        }
        self.state.reset();
        if self.active_for(origin, user_id).is_some() {
            self.close_active().await;
        }
        self.local.wipe(origin, user_id).await
    }

    /// Unsent messages of `(origin, user_id)` if their stores are open (the sign-out warning).
    pub(crate) async fn unsent(&self, origin: &str, user_id: &str) -> u64 {
        match self.active_for(origin, user_id) {
            Some(a) => a.outbox.unsent_count().await.unwrap_or(0),
            None => 0,
        }
    }

    /// Other users with data on this device than `(origin, user_id)` (#46 §8). The app reads
    /// them with their counts ([`Offline::others_with_unsent`]); tests read them plain.
    #[cfg(test)]
    pub(crate) async fn others(
        &self,
        origin: &str,
        user_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        self.local.others(origin, user_id).await
    }

    /// As [`Offline::others`], each with its unsent count (`None`: unreadable).
    pub(crate) async fn others_with_unsent(
        &self,
        origin: &str,
        user_id: &str,
    ) -> Result<Vec<(String, String, Option<u64>)>, StoreError> {
        self.local.others_with_unsent(origin, user_id).await
    }

    /// Wipe every other user's data (after the app surfaced it).
    pub(crate) async fn wipe_others(
        &mut self,
        origin: &str,
        user_id: &str,
    ) -> Result<(), StoreError> {
        for (o, u) in self.local.others(origin, user_id).await? {
            if self
                .active
                .as_ref()
                .is_some_and(|a| a.origin == o && a.user_id == u)
            {
                self.close_active().await;
            }
            self.local.wipe(&o, &u).await?;
        }
        Ok(())
    }
}

/// Feeds a user's cache: raw WebSocket events through the guard, a `/sync` on every
/// (re)connect (`ready`) and periodically, hints as asked. Cache notices go to `events`.
async fn pump(
    cache: Arc<Cache>,
    events: broadcast::Sender<CacheEvent>,
    mut raw: broadcast::Receiver<(String, Value)>,
) {
    let mut notices = cache.events();
    // Stopped however the pump ends, an abort included.
    let _forward = {
        let events = events.clone();
        AbortOnDrop(tokio::spawn(async move {
            loop {
                match notices.recv().await {
                    Ok(e) => {
                        let _ = events.send(e);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let _ = events.send(CacheEvent::Reset); // re-read everything
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }))
    };
    let mut tick = tokio::time::interval(PERIODIC_SYNC);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            got = raw.recv() => match got {
                Ok((ty, data)) => match ty.as_str() {
                    "ready" => cache.schedule_sync(),
                    "sync.hint" => {
                        if let Some(seq) = data.get("seq").and_then(Value::as_i64) {
                            cache.hint(seq).await;
                        }
                    }
                    _ => cache.live_event(&ty, &data).await,
                },
                // Missed events: a /sync catches up whatever they were.
                Err(broadcast::error::RecvError::Lagged(_)) => cache.schedule_sync(),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = tick.tick() => cache.schedule_sync(),
        }
    }
}
