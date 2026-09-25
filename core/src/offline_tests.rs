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
    async fn send(&self, ch: &str, body: &str, cid: &str, _e: u64) -> Result<Value, SendFailure> {
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
    outbox.enqueue("c", "later", None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        s.fake.sent.lock().unwrap().is_empty(),
        "sent while signed out"
    );
    assert_eq!(s.offline.unsent().await, 1);
    sign_in(&mut s, "u1", 2).await; // the same user again: same stores, new session
    let fake = s.fake.clone();
    eventually("the send", move || !fake.sent.lock().unwrap().is_empty()).await;
}

/// "Remove this device's data": erased locally; nothing of it stays.
#[tokio::test]
async fn forgetting_erases_the_users_stores() {
    let mut s = setup().await;
    sign_in(&mut s, "u1", 1).await;
    s.offline.forget_active().await.unwrap();
    assert!(s.offline.active().is_none());
    let dirs: Vec<_> = std::fs::read_dir(s.root.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().unwrap().is_dir())
        .collect();
    assert!(dirs.is_empty(), "a store directory survived");
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
    outbox.enqueue("c", "hello", None).await.unwrap();
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
}
