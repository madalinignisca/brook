//! The per-user lifecycle (plan C5): stores open on sign-in, the WebSocket feeds the cache,
//! the outbox pauses when signed out, "Remove this device's data" erases first, and another
//! user's data is found and wiped.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::cache::{CacheEvent, History};
use crate::local::LocalData;
use crate::offline::{Net, Offline};
use crate::outbox::{Post, SendFailure};
use crate::sync::{Fetch, Page};
use crate::{InMemoryKeySlot, KeySlot};

#[derive(Default)]
struct Fake {
    syncs: AtomicUsize,
    sent: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Fetch for Fake {
    async fn page(&self, _since: &str) -> Result<Page, crate::Error> {
        self.syncs.fetch_add(1, Ordering::SeqCst);
        Ok(Page::Rows(json!({
            "channels": [{ "id": "c", "name": "general", "seq": 3 }],
            "removed_channels": [], "left_members": [], "users": [], "messages": [],
            "memberships": [{ "channel_id": "c", "user_id": "u1", "seq": 3 }],
            "next": "5", "more": false,
        })))
    }
}

#[async_trait::async_trait]
impl History for Fake {
    async fn page(&self, _: &str, _: Option<&str>, _: usize) -> Result<Vec<Value>, crate::Error> {
        Ok(vec![])
    }
}

#[async_trait::async_trait]
impl Post for Fake {
    async fn send(
        &self,
        ch: &str,
        msg: &crate::outbox::Outgoing,
        cid: &str,
        _e: u64,
    ) -> Result<Value, SendFailure> {
        let body = msg.body.as_str();
        self.sent.lock().unwrap().push(body.to_string());
        Ok(
            json!({ "id": format!("m-{cid}"), "channel_id": ch, "author_id": "u1", "body": body,
                   "created_at": "2026-09-25T10:00:00Z", "seq": 50, "client_id": cid }),
        )
    }
}

struct Setup {
    offline: Offline,
    fake: Arc<Fake>,
    raw: broadcast::Sender<(String, Value)>,
    root: tempfile::TempDir,
}

async fn setup() -> Setup {
    let root = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let local = LocalData::open(root.path(), slot.clone() as Arc<dyn KeySlot>)
        .await
        .unwrap()
        .unwrap();
    let (raw, _) = broadcast::channel(64);
    Setup {
        offline: Offline::new(local),
        fake: Arc::new(Fake::default()),
        raw,
        root,
    }
}

fn net(f: &Arc<Fake>) -> Net {
    Net {
        fetch: f.clone(),
        history: f.clone(),
        post: f.clone(),
    }
}

async fn sign_in(s: &mut Setup, user: &str, epoch: u64) -> bool {
    let n = net(&s.fake);
    s.offline
        .signed_in("https://a", user, epoch, n, s.raw.subscribe())
        .await
        .unwrap()
}

async fn eventually(what: &str, f: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_connect_syncs_and_live_events_land_in_the_cache() {
    let mut s = setup().await;
    assert!(sign_in(&mut s, "u1", 1).await);
    let fake = s.fake.clone();
    eventually("the sync at sign-in", move || {
        fake.syncs.load(Ordering::SeqCst) >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(700)).await; // past the debounce
    let _ = s.raw.send(("ready".into(), Value::Null));
    let fake = s.fake.clone();
    eventually("the sync on connect", move || {
        fake.syncs.load(Ordering::SeqCst) >= 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = s.raw.send((
        "message.new".into(),
        json!({ "id": "m1", "channel_id": "c", "author_id": "bob", "body": "hi",
                "created_at": "2026-09-25T10:00:00Z", "seq": 6 }),
    ));
    let cache = s.offline.active().unwrap().cache.clone();
    for _ in 0..500 {
        if !cache
            .cached_messages("c", None, 10)
            .await
            .unwrap()
            .messages
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let page = cache.cached_messages("c", None, 10).await.unwrap();
    assert_eq!(page.messages[0]["body"], "hi");
}

#[tokio::test]
async fn a_hint_above_the_cursor_syncs() {
    let mut s = setup().await;
    sign_in(&mut s, "u1", 1).await;
    let fake = s.fake.clone();
    eventually("the sync at sign-in", move || {
        fake.syncs.load(Ordering::SeqCst) >= 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    let _ = s.raw.send(("sync.hint".into(), json!({ "seq": 99 })));
    let fake = s.fake.clone();
    eventually("the hinted sync", move || {
        fake.syncs.load(Ordering::SeqCst) >= 2
    })
    .await;
}

#[tokio::test]
async fn signed_out_the_outbox_waits_and_the_next_sign_in_sends() {
    let mut s = setup().await;
    sign_in(&mut s, "u1", 1).await;
    s.offline.signed_out();
    let outbox = s.offline.active().unwrap().outbox.clone();
    outbox.enqueue("c", "later", None, None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        s.fake.sent.lock().unwrap().is_empty(),
        "sent while signed out"
    );
    assert_eq!(s.offline.unsent("https://a", "u1").await, 1);
    sign_in(&mut s, "u1", 2).await; // the same user again: same stores, new session
    let fake = s.fake.clone();
    eventually("the send", move || !fake.sent.lock().unwrap().is_empty()).await;
}

/// "Remove this device's data": erased locally; nothing of it stays.
#[tokio::test]
async fn forgetting_erases_the_users_stores() {
    let mut s = setup().await;
    sign_in(&mut s, "u1", 1).await;
    s.offline.forget("https://a", "u1", 1).await.unwrap();
    assert!(s.offline.active().is_none());
    let dirs: Vec<_> = std::fs::read_dir(s.root.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().unwrap().is_dir())
        .collect();
    assert!(dirs.is_empty(), "a store directory survived");
}

/// A sign-in still queued from before "Remove this device's data" doesn't bring the stores
/// back; the next sign-in (a later epoch) does.
#[tokio::test]
async fn a_forgotten_user_stays_forgotten_until_a_new_sign_in() {
    let mut s = setup().await;
    sign_in(&mut s, "u1", 3).await;
    s.offline.forget("https://a", "u1", 3).await.unwrap();
    assert!(
        !sign_in(&mut s, "u1", 3).await,
        "the wiped stores came back"
    );
    assert!(s.offline.active().is_none());
    assert!(sign_in(&mut s, "u1", 5).await);
}

/// Forgetting a user whose stores never opened here still erases them.
#[tokio::test]
async fn forgetting_erases_stores_that_are_not_open() {
    let mut s = setup().await;
    sign_in(&mut s, "u1", 1).await;
    sign_in(&mut s, "u2", 2).await; // u1's stores closed, still on disk
    s.offline.forget("https://a", "u1", 2).await.unwrap();
    assert!(s
        .offline
        .others("https://a", "u2")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(s.offline.active().unwrap().user_id, "u2");
}

/// Another user signs in: the first user's stores close and are listed, then wiped.
#[tokio::test]
async fn another_users_data_is_found_and_wiped() {
    let mut s = setup().await;
    sign_in(&mut s, "u1", 1).await;
    sign_in(&mut s, "u2", 2).await;
    assert_eq!(s.offline.active().unwrap().user_id, "u2");
    let others = s.offline.others("https://a", "u2").await.unwrap();
    assert_eq!(others, vec![("https://a".to_string(), "u1".to_string())]);
    s.offline.wipe_others("https://a", "u2").await.unwrap();
    assert!(s
        .offline
        .others("https://a", "u2")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        s.offline.active().unwrap().user_id,
        "u2",
        "the active user was wiped"
    );
}

#[tokio::test]
async fn cache_notices_reach_the_app() {
    let mut s = setup().await;
    let mut events = s.offline.events();
    sign_in(&mut s, "u1", 1).await;
    let outbox = s.offline.active().unwrap().outbox.clone();
    outbox.enqueue("c", "hello", None, None).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(CacheEvent::Outbox(ch)) = events.recv().await {
                return ch;
            }
        }
    })
    .await
    .expect("no outbox notice");
    assert_eq!(got, "c");
}

// ---- The client-level state feed and lost messages ----

async fn synced(rx: &mut tokio::sync::watch::Receiver<crate::cache::CacheState>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while rx.borrow_and_update().last_synced.is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the feed never showed a sync"
        );
        let _ = tokio::time::timeout(Duration::from_millis(50), rx.changed()).await;
    }
}

/// Signed in, the feed follows the cache; signed out, it's the default at once; signed in
/// again, it follows again.
#[tokio::test]
async fn the_state_feed_follows_the_signed_in_user() {
    let mut s = setup().await;
    let mut feed = s.offline.state_feed();
    sign_in(&mut s, "u1", 1).await;
    synced(&mut feed).await;
    s.offline.signed_out();
    assert_eq!(
        *feed.borrow(),
        crate::cache::CacheState::default(),
        "signed out, the feed still showed the user's state"
    );
    sign_in(&mut s, "u1", 2).await;
    synced(&mut feed).await;
}

/// A sign-out and sign-in of one user that reach the watcher as one change (no
/// `signed_out` between) still reset the feed.
#[tokio::test]
async fn a_new_epoch_for_the_same_user_resets_the_feed() {
    let mut s = setup().await;
    let mut feed = s.offline.state_feed();
    sign_in(&mut s, "u1", 1).await;
    synced(&mut feed).await;
    feed.borrow_and_update();
    sign_in(&mut s, "u1", 3).await;
    assert!(
        feed.has_changed().unwrap(),
        "the feed carried on across the new session"
    );
}

/// Forgetting the session's user while another user's stores are still open (a switch
/// the watcher hasn't reached) stops the feed showing that other user at once.
#[tokio::test]
async fn forgetting_resets_the_feed_whoever_is_open() {
    let mut s = setup().await;
    let mut feed = s.offline.state_feed();
    sign_in(&mut s, "u1", 1).await;
    synced(&mut feed).await;
    s.offline.forget("https://a", "u2", 2).await.unwrap();
    assert_eq!(
        *feed.borrow(),
        crate::cache::CacheState::default(),
        "u1's state still showed after u2 was forgotten"
    );
    let cache = s.offline.active().unwrap().cache.clone();
    tokio::time::sleep(Duration::from_millis(700)).await;
    cache.sync_now().await.unwrap();
    assert_eq!(*feed.borrow(), crate::cache::CacheState::default());
}

/// A forwarder whose generation has passed writes nothing, even while its cache still
/// changes (the window between a switch and the old forwarder stopping).
#[tokio::test]
async fn a_stale_forwarder_never_overwrites_the_reset() {
    let mut s = setup().await;
    sign_in(&mut s, "u1", 1).await;
    let cache = s.offline.active().unwrap().cache.clone();
    let feed = crate::offline::StateFeed::new(
        tokio::sync::watch::channel(crate::cache::CacheState::default()).0,
    );
    let rx = feed.subscribe();
    let _old = feed.follow(&cache); // kept running on purpose
    feed.reset();
    tokio::time::sleep(Duration::from_millis(700)).await; // past the sign-in's sync
    cache.sync_now().await.unwrap();
    assert_eq!(
        *rx.borrow(),
        crate::cache::CacheState::default(),
        "the old cache's state reached the feed after the reset"
    );
}

/// Losses are numbered: acknowledging one clears it, but a loss that happened after the
/// app read the number stays reported.
#[test]
fn acknowledging_a_loss_keeps_a_newer_one() {
    let losses = std::sync::Mutex::new(crate::offline::Losses::default());
    let (events, _) = broadcast::channel(4);
    assert_eq!(losses.lock().unwrap().current(), None);
    crate::offline::record_loss(&losses, &events);
    let seen = losses.lock().unwrap().current().unwrap();
    crate::offline::record_loss(&losses, &events); // meanwhile
    losses.lock().unwrap().acknowledge(seen);
    let newer = losses.lock().unwrap().current();
    assert!(
        newer.is_some_and(|n| n > seen),
        "the newer loss was cleared"
    );
    losses.lock().unwrap().acknowledge(newer.unwrap());
    assert_eq!(losses.lock().unwrap().current(), None);
}

/// An outbox in the format before this one (no reply column) is lost on upgrade: the app
/// is told through the numbered loss when the user signs in.
#[tokio::test]
async fn an_old_outbox_is_reported_lost_when_its_user_signs_in() {
    let root = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    {
        let local = LocalData::open(root.path(), slot.clone() as Arc<dyn KeySlot>)
            .await
            .unwrap()
            .unwrap();
        let u = local.open_user("https://a", "u1").await.unwrap();
        if let crate::store::Opened::Ready { db, .. } = u.cache {
            db.close().await;
        }
        let crate::store::Opened::Ready { db: outbox, .. } = u.outbox else {
            panic!("outbox not ready");
        };
        outbox
            .call(|c| c.execute("UPDATE meta SET format = 1", []))
            .await
            .unwrap();
        outbox.close().await;
        local.close().await;
    }
    let local = LocalData::open(root.path(), slot.clone() as Arc<dyn KeySlot>)
        .await
        .unwrap()
        .unwrap();
    let (raw, _) = broadcast::channel(64);
    let mut s = Setup {
        offline: Offline::new(local),
        fake: Arc::new(Fake::default()),
        raw,
        root,
    };
    assert_eq!(s.offline.losses(), None);
    assert!(
        sign_in(&mut s, "u1", 1).await,
        "the rebuilt outbox didn't open"
    );
    assert!(
        s.offline.losses().is_some(),
        "the lost outbox went unreported"
    );
}

// ---- Through BrookClient (the auth watcher wiring) ----

mod client {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::test_support::TestServer;
    use crate::{BrookClient, CoreConfig, InMemoryKeySlot, KeySlot, LoginOutcome};

    async fn active(c: &BrookClient) -> bool {
        for _ in 0..300 {
            if c.other_local_users().await.is_ok() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// Enabled, then signed in: the user's stores open; "Remove this device's data" erases
    /// them locally before signing out; reads then say local data is unavailable.
    #[tokio::test]
    async fn sign_in_opens_the_stores_and_forget_erases_them() {
        let server = TestServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let slot: Arc<dyn KeySlot> = Arc::new(InMemoryKeySlot::default());
        let c = BrookClient::new(CoreConfig::new(&server.base).unwrap()).unwrap();
        assert!(c.enable_local_data(slot, dir.path().to_path_buf()).await);
        assert!(
            c.cached_channels().await.is_err(),
            "open before anyone signed in"
        );
        assert!(matches!(
            c.login("alice", "pw").await.unwrap(),
            LoginOutcome::LoggedIn(_)
        ));
        assert!(
            active(&c).await,
            "the stores never opened for the signed-in user"
        );
        assert!(c.cached_channels().await.unwrap().is_empty());
        let stores = dir.path().join("stores");
        let store_dirs = || {
            std::fs::read_dir(&stores)
                .unwrap()
                .flatten()
                .filter(|e| e.file_type().unwrap().is_dir())
                .count()
        };
        assert_eq!(store_dirs(), 1);
        c.sign_out_and_forget().await.unwrap();
        assert_eq!(
            store_dirs(),
            0,
            "the user's data survived \"Remove this device's data\""
        );
        assert!(c.cached_channels().await.is_err());
    }

    /// Between a switch of user and the watcher catching up, the previous user's stores are
    /// still open: they answer nobody else. (tokio's mutex is FIFO, so the read below gets
    /// the lock before the watcher does.)
    #[tokio::test]
    async fn open_stores_answer_only_their_own_user() {
        let server = TestServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let slot: Arc<dyn KeySlot> = Arc::new(InMemoryKeySlot::default());
        let c = Arc::new(BrookClient::new(CoreConfig::new(&server.base).unwrap()).unwrap());
        assert!(c.enable_local_data(slot, dir.path().to_path_buf()).await);
        c.login("alice", "pw").await.unwrap();
        assert!(active(&c).await);
        let held = c.offline.lock().await;
        let read = tokio::spawn({
            let c = c.clone();
            async move { c.cached_channels().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut other = c.session.snapshot().await.1.unwrap();
        other.user.id = "someone-else".into();
        c.session.replace(Some(other)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(held);
        assert!(
            read.await.unwrap().is_err(),
            "another user read the open stores"
        );
    }

    /// Unsent messages lost while the local data was being opened (before any listener
    /// existed) are still reported afterwards, until acknowledged.
    #[tokio::test]
    async fn a_loss_found_while_enabling_is_kept_for_the_app() {
        let server = TestServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let slot = Arc::new(InMemoryKeySlot::default());
        let stores = dir.path().join("stores");
        {
            let local = crate::local::LocalData::open(&stores, slot.clone() as Arc<dyn KeySlot>)
                .await
                .unwrap()
                .unwrap();
            let u = local.open_user("https://a", "u1").await.unwrap();
            if let crate::store::Opened::Ready { db, .. } = u.cache {
                db.close().await;
            }
            let crate::store::Opened::Ready { db: outbox, .. } = u.outbox else {
                panic!("outbox not ready");
            };
            outbox
                .call(|c| {
                    c.execute(
                        "INSERT INTO outbox(client_id, channel_id, body, state, created_at)
                         VALUES ('x', 'c', 'hi', 'queued', 'now')",
                        [],
                    )
                })
                .await
                .unwrap();
            outbox.close().await;
            local.close().await;
        }
        slot.put("index", vec![5; 32]); // the index key is lost
        let c = BrookClient::new(CoreConfig::new(&server.base).unwrap()).unwrap();
        assert!(c.enable_local_data(slot, dir.path().to_path_buf()).await);
        let n = c.outbox_lost().expect("the loss wasn't kept");
        c.acknowledge_outbox_lost(n);
        assert_eq!(c.outbox_lost(), None);
    }

    /// Dropping the client closes the open stores and the index: once it has, every file
    /// opens again at once (nothing is left to whenever the last handle drops).
    #[tokio::test]
    async fn dropping_the_client_closes_the_stores() {
        let server = TestServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let slot: Arc<dyn KeySlot> = Arc::new(InMemoryKeySlot::default());
        let c = BrookClient::new(CoreConfig::new(&server.base).unwrap()).unwrap();
        assert!(
            c.enable_local_data(slot.clone(), dir.path().to_path_buf())
                .await
        );
        c.login("alice", "pw").await.unwrap();
        assert!(active(&c).await);
        let user = c.session.snapshot().await.1.unwrap().user.id;
        let origin = server.base.trim_end_matches('/').to_string();
        let offline = c.offline.clone();
        drop(c);
        let mut closed = false;
        for _ in 0..300 {
            if offline.lock().await.is_none() {
                closed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            closed,
            "the local data stayed open after the client went away"
        );
        let local = crate::local::LocalData::open(&dir.path().join("stores"), slot)
            .await
            .expect("the index is still open")
            .unwrap();
        let stores = local.open_user(&origin, &user).await.unwrap();
        assert!(
            matches!(
                stores.cache,
                crate::store::Opened::Ready { rebuilt: None, .. }
            ),
            "the cache is still open"
        );
    }
}
