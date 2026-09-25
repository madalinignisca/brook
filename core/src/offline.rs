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

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{broadcast, watch};

use crate::cache::{Cache, CacheEvent, History};
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
}

/// One user's open stores and the tasks feeding them.
pub(crate) struct Active {
    pub(crate) origin: String,
    pub(crate) user_id: String,
    pub(crate) cache: Arc<Cache>,
    pub(crate) outbox: Arc<Outbox>,
    session: watch::Sender<Option<u64>>,
    pump: tokio::task::JoinHandle<()>,
}

pub(crate) struct Offline {
    local: LocalData,
    active: Option<Active>,
    /// One notice stream for the app, whoever is signed in.
    events: broadcast::Sender<CacheEvent>,
}

impl Offline {
    pub(crate) fn new(local: LocalData) -> Self {
        let (events, _) = broadcast::channel(512);
        Self {
            local,
            active: None,
            events,
        }
    }

    pub(crate) fn events(&self) -> broadcast::Receiver<CacheEvent> {
        self.events.subscribe()
    }

    pub(crate) fn active(&self) -> Option<&Active> {
        self.active.as_ref()
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
        if let Some(a) = &self.active {
            if a.origin == origin && a.user_id == user_id {
                a.session.send_replace(Some(epoch));
                return Ok(true);
            }
        }
        self.close_active().await;
        let stores = self.local.open_user(origin, user_id).await?;
        if stores.outbox_lost {
            let _ = self.events.send(CacheEvent::OutboxLost);
        }
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
        let outbox = Outbox::open(outbox_db, cache.clone(), net.post, session_rx).await?;
        outbox.set_events(self.events.clone());
        let _ = outbox.resume().await;
        let pump = tokio::spawn(pump(cache.clone(), self.events.clone(), raw));
        self.active = Some(Active {
            origin: origin.to_string(),
            user_id: user_id.to_string(),
            cache,
            outbox,
            session,
            pump,
        });
        Ok(true)
    }

    /// Signed out, keeping the data: the outbox pauses, the stores stay open for reads.
    pub(crate) fn signed_out(&self) {
        if let Some(a) = &self.active {
            a.session.send_replace(None);
        }
    }

    async fn close_active(&mut self) {
        if let Some(a) = self.active.take() {
            a.session.send_replace(None);
            a.pump.abort();
            let _ = a.pump.await;
            a.outbox.close().await;
            a.cache.close().await;
        }
    }

    /// "Remove this device's data": close and erase the active user's stores, locally and
    /// first (the caller signs out of the server afterwards, whether or not that works).
    pub(crate) async fn forget_active(&mut self) -> Result<(), StoreError> {
        let Some(a) = &self.active else {
            return Ok(());
        };
        let (origin, user_id) = (a.origin.clone(), a.user_id.clone());
        self.close_active().await;
        self.local.wipe(&origin, &user_id).await
    }

    /// Unsent messages of the active user (the sign-out warning).
    pub(crate) async fn unsent(&self) -> u64 {
        match &self.active {
            Some(a) => a.outbox.unsent_count().await.unwrap_or(0),
            None => 0,
        }
    }

    /// Other users with data on this device than `(origin, user_id)` (#46 §8).
    pub(crate) async fn others(
        &self,
        origin: &str,
        user_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        self.local.others(origin, user_id).await
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

    pub(crate) async fn close(mut self) {
        self.close_active().await;
        self.local.close().await;
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
    let forward = {
        let events = events.clone();
        tokio::spawn(async move {
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
        })
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
    forward.abort();
}
