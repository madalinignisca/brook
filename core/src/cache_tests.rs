//! The cache's front (plan C3): reads, change notices after commit, unread counts, the
//! network-needed signal, and the sync triggers (hints, unappliable events, single-flight).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::cache::{Cache, CacheEvent, History};
use crate::store::{self, Kind, Opened};
use crate::sync::{Fetch, Page};
use crate::{InMemoryKeySlot, KeySlot, KeyStore};

const ME: &str = "me";

/// `/sync` answers: a queue of pages (the last one repeats), counting calls, optionally held.
struct Server {
    pages: Mutex<Vec<Value>>,
    calls: AtomicUsize,
    gate: Option<Arc<Notify>>,
}

#[async_trait::async_trait]
impl Fetch for Server {
    async fn page(&self, _since: &str) -> Result<Page, crate::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(g) = &self.gate {
            g.notified().await;
        }
        let mut pages = self.pages.lock().unwrap();
        let p = if pages.len() > 1 {
            pages.remove(0)
        } else {
            pages[0].clone()
        };
        Ok(Page::Rows(p))
    }
}

struct Offline;

#[async_trait::async_trait]
impl Fetch for Offline {
    async fn page(&self, _since: &str) -> Result<Page, crate::Error> {
        Err(crate::Error::UnexpectedResponse)
    }
}

/// History pages from a fixed list of message ids (newest first), optionally held.
struct Hist {
    ids: Vec<&'static str>,
    gate: Option<Arc<Notify>>,
}

#[async_trait::async_trait]
impl History for Hist {
    async fn page(
        &self,
        channel_id: &str,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>, crate::Error> {
        if let Some(g) = &self.gate {
            g.notified().await;
        }
        Ok(self
            .ids
            .iter()
            .filter(|id| before.is_none_or(|b| **id < b))
            .take(limit)
            .map(|id| msg(id, channel_id, 5, "bob", "old"))
            .collect())
    }
}

fn msg(id: &str, channel: &str, seq: i64, author: &str, body: &str) -> Value {
    json!({ "id": id, "channel_id": channel, "author_id": author, "body": body,
            "created_at": "2026-09-25T10:00:00Z", "seq": seq })
}

fn page(cursor: i64, last_read: Option<&str>, messages: Vec<Value>, users: Vec<Value>) -> Value {
    json!({
        "channels": [{ "id": "c", "name": "general", "seq": 3 }],
        "removed_channels": [],
        "memberships": [
            { "channel_id": "c", "user_id": ME, "seq": 3, "last_read_message_id": last_read },
            { "channel_id": "c", "user_id": "bob", "seq": 3 }
        ],
        "left_members": [],
        "users": users,
        "messages": messages,
        "next": cursor.to_string(),
        "more": false,
    })
}

struct Setup {
    cache: Arc<Cache>,
    server: Arc<Server>,
    _dir: tempfile::TempDir,
}

fn setup(pages: Vec<Value>, hist: Hist, gate: Option<Arc<Notify>>) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let slot: Arc<dyn KeySlot> = Arc::new(InMemoryKeySlot::default());
    let db = match store::open(dir.path(), Kind::Cache, "s", &KeyStore::new(slot)).unwrap() {
        Opened::Ready { db, .. } => db,
        other => panic!("{other:?}"),
    };
    let server = Arc::new(Server {
        pages: Mutex::new(pages),
        calls: AtomicUsize::new(0),
        gate,
    });
    let cache = Cache::new(db, ME.into(), server.clone(), Arc::new(hist));
    Setup {
        cache,
        server,
        _dir: dir,
    }
}

fn no_history() -> Hist {
    Hist {
        ids: vec![],
        gate: None,
    }
}

#[tokio::test]
async fn unread_counts_others_messages_after_my_read_marker() {
    let s = setup(
        vec![page(
            9,
            Some("m2"),
            vec![
                msg("m1", "c", 4, "bob", "a"),
                msg("m2", "c", 5, "bob", "b"),
                msg("m3", "c", 6, "bob", "c"),
                msg("m4", "c", 7, ME, "mine"),
                msg("m5", "c", 8, "bob", "d"),
            ],
            vec![],
        )],
        no_history(),
        None,
    );
    s.cache.sync_now().await.unwrap();
    s.cache
        .live_event(
            "message.delete",
            &json!({ "id": "m5", "channel_id": "c", "seq": 10 }),
        )
        .await;
    let channels = s.cache.cached_channels().await.unwrap();
    assert_eq!(channels.len(), 1);
    assert_eq!(
        channels[0].unread, 1,
        "m3 only: not read, not mine, not deleted"
    );
}

#[tokio::test]
async fn change_notices_follow_the_commit() {
    let s = setup(
        vec![page(
            9,
            None,
            vec![msg("m1", "c", 4, "bob", "a")],
            vec![json!({ "id": "bob", "seq": 4 })],
        )],
        no_history(),
        None,
    );
    let mut events = s.cache.events();
    s.cache.sync_now().await.unwrap();
    let mut got = vec![];
    while let Ok(e) = events.try_recv() {
        got.push(e);
    }
    assert!(
        got.contains(&CacheEvent::Channels(vec!["c".into()])),
        "{got:?}"
    );
    assert!(
        got.contains(&CacheEvent::Users(vec!["bob".into()])),
        "{got:?}"
    );
    // The notice came after the commit: the channel reads back already.
    assert_eq!(s.cache.cached_channels().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_never_opened_channel_needs_the_network_and_then_doesnt() {
    let s = setup(
        vec![page(9, None, vec![], vec![])],
        Hist {
            ids: vec!["m9", "m8", "m7", "m6", "m5"],
            gate: None,
        },
        None,
    );
    s.cache.sync_now().await.unwrap();
    let p = s.cache.cached_messages("c", None, 3).await.unwrap();
    assert!(p.needs_network && p.messages.is_empty());
    s.cache.load_head("c", 3).await.unwrap();
    let p = s.cache.cached_messages("c", None, 3).await.unwrap();
    assert!(!p.needs_network);
    assert_eq!(ids(&p.messages), vec!["m9", "m8", "m7"]);
    // Below the range: the network again, until the start is known.
    let p = s.cache.cached_messages("c", Some("m7"), 3).await.unwrap();
    assert!(p.needs_network);
    s.cache.load_older("c", 3).await.unwrap();
    let p = s.cache.cached_messages("c", Some("m7"), 3).await.unwrap();
    assert_eq!(ids(&p.messages), vec!["m6", "m5"]);
    assert!(!p.needs_network, "a short page from the start is complete");
}

fn ids(v: &[Value]) -> Vec<&str> {
    v.iter().map(|m| m["id"].as_str().unwrap()).collect()
}

/// Waits (real time, bounded) until `f` holds; panics instead of hanging.
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

/// Lets any scheduled sync (debounce + run) finish, in real time.
async fn settle() {
    tokio::time::sleep(crate::cache::HINT_DEBOUNCE * 3).await;
}

#[tokio::test]
async fn a_burst_of_hints_costs_one_sync() {
    let s = setup(vec![page(9, None, vec![], vec![])], no_history(), None);
    s.cache.sync_now().await.unwrap();
    for seq in 10..15 {
        s.cache.hint(seq).await;
    }
    settle().await;
    assert_eq!(
        s.server.calls.load(Ordering::SeqCst),
        2,
        "one sync for five hints"
    );
}

#[tokio::test]
async fn a_hint_the_cache_already_has_costs_nothing() {
    let s = setup(vec![page(9, None, vec![], vec![])], no_history(), None);
    s.cache.sync_now().await.unwrap();
    s.cache.hint(9).await; // at the cursor
    s.cache.hint(5).await; // below it
    settle().await;
    assert_eq!(s.server.calls.load(Ordering::SeqCst), 1);
}

/// A reaction's summary is per viewer: the live event isn't applied, it asks for a sync.
#[tokio::test]
async fn an_event_that_cant_be_applied_asks_for_a_sync() {
    let s = setup(vec![page(9, None, vec![], vec![])], no_history(), None);
    s.cache.sync_now().await.unwrap();
    let reaction = json!({ "message_id": "m1", "channel_id": "c", "emoji": "👍",
                           "user_id": "bob", "added": true, "count": 1, "seq": 12 });
    s.cache.live_event("reaction.update", &reaction).await;
    let server = s.server.clone();
    eventually("the sync a reaction asks for", move || {
        server.calls.load(Ordering::SeqCst) == 2
    })
    .await;
}

/// Asked while a run is going (it may already have fetched past the change): the run goes
/// round once more, and only one runs at a time.
#[tokio::test]
async fn a_sync_asked_during_a_run_runs_again_after_it() {
    let gate = Arc::new(Notify::new());
    let s = setup(
        vec![page(9, None, vec![], vec![])],
        no_history(),
        Some(gate.clone()),
    );
    let first = tokio::spawn({
        let c = s.cache.clone();
        async move { c.sync_now().await }
    });
    let server = s.server.clone();
    eventually("the first run", move || {
        server.calls.load(Ordering::SeqCst) == 1
    })
    .await;
    s.cache.sync_now().await.unwrap(); // returns at once: the running one will go again
    assert_eq!(s.server.calls.load(Ordering::SeqCst), 1, "two runs at once");
    gate.notify_one();
    let server = s.server.clone();
    eventually("the second run", move || {
        server.calls.load(Ordering::SeqCst) == 2
    })
    .await;
    gate.notify_one();
    tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn offline_is_reported_and_reads_still_work() {
    let dir = tempfile::tempdir().unwrap();
    let slot: Arc<dyn KeySlot> = Arc::new(InMemoryKeySlot::default());
    let db = match store::open(dir.path(), Kind::Cache, "s", &KeyStore::new(slot)).unwrap() {
        Opened::Ready { db, .. } => db,
        other => panic!("{other:?}"),
    };
    let cache = Cache::new(db, ME.into(), Arc::new(Offline), Arc::new(no_history()));
    assert!(cache.sync_now().await.is_err());
    assert!(cache.state().borrow().offline);
    assert!(cache.cached_channels().await.unwrap().is_empty());
}

/// A history page out while the caller is removed: it lands nowhere, and adds no coverage.
#[tokio::test]
async fn a_history_page_that_straddles_a_removal_lands_nowhere() {
    let gate = Arc::new(Notify::new());
    let s = setup(
        vec![page(9, None, vec![], vec![])],
        Hist {
            ids: vec!["m9", "m8"],
            gate: Some(gate.clone()),
        },
        None,
    );
    s.cache.sync_now().await.unwrap();
    let load = tokio::spawn({
        let c = s.cache.clone();
        async move { c.load_head("c", 5).await }
    });
    tokio::task::yield_now().await;
    s.cache
        .live_event("channel.delete", &json!({ "id": "c", "seq": 20 }))
        .await;
    gate.notify_one();
    load.await.unwrap().unwrap();
    let p = s.cache.cached_messages("c", None, 5).await.unwrap();
    assert!(p.messages.is_empty());
    assert!(p.needs_network, "coverage from a page that didn't land");
}

// ---- HTTP side ----

mod http {
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::watch;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cache::History;
    use crate::cache_http::Http;
    use crate::session_store::SessionStore;
    use crate::sync::{Fetch, Page};
    use crate::{AuthState, Session, User};

    async fn http(server: &MockServer) -> Http {
        let (tx, _) = watch::channel(AuthState::LoggedOut);
        let session = SessionStore::new(Arc::new(tx));
        session
            .replace(Some(Session {
                access_token: "tok".into(),
                refresh_token: "ref".into(),
                user: User {
                    id: "me".into(),
                    handle: "me".into(),
                    display_name: "Me".into(),
                    global_role: "member".into(),
                },
            }))
            .await;
        Http {
            http: reqwest::Client::new(),
            base: format!("{}/", server.uri()).parse().unwrap(),
            session,
        }
    }

    #[tokio::test]
    async fn sync_sends_the_cursor_with_the_token_and_410_is_a_reset() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/sync"))
            .and(query_param("since", "42"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "next": "43" })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/sync"))
            .and(query_param("since", "999"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&server)
            .await;
        let h = http(&server).await;
        assert!(matches!(Fetch::page(&h, "42").await.unwrap(), Page::Rows(v) if v["next"] == "43"));
        assert!(matches!(Fetch::page(&h, "999").await.unwrap(), Page::Reset));
    }

    #[tokio::test]
    async fn errors_carry_the_status_never_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/sync"))
            .respond_with(ResponseTemplate::new(502).set_body_string("tok leaked by a proxy"))
            .mount(&server)
            .await;
        let h = http(&server).await;
        let err = match Fetch::page(&h, "1").await {
            Err(e) => e,
            Ok(_) => panic!("a 502 was a page"),
        };
        assert!(!err.to_string().contains("tok"), "{err}");
    }

    #[tokio::test]
    async fn history_asks_within_the_server_cap() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/channels/c1/messages"))
            .and(query_param("limit", "100"))
            .and(query_param("before", "m9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([{ "id": "m1" }])))
            .mount(&server)
            .await;
        let h = http(&server).await;
        let rows = History::page(&h, "c1", Some("m9"), 500).await.unwrap();
        assert_eq!(rows.len(), 1);
    }
}
