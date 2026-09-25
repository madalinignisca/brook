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
    since: Mutex<Vec<String>>,
    gate: Option<Arc<Notify>>,
}

#[async_trait::async_trait]
impl Fetch for Server {
    async fn page(&self, since: &str) -> Result<Page, crate::Error> {
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
        self.since.lock().unwrap().push(since.to_string());
        if p == json!("RESET") {
            return Ok(Page::Reset); // `410 sync.reset`
        }
        Ok(Page::Rows(p))
    }
}

struct Offline;

#[async_trait::async_trait]
impl Fetch for Offline {
    async fn page(&self, _since: &str) -> Result<Page, crate::Error> {
        Err(crate::Error::Timeout)
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
        since: Mutex::new(vec![]),
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
            transfers: Arc::new(crate::transfer::Transfers::new()),
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

// ---- Review round 1 ----

/// A live message above the range's top may follow a missed one: not complete.
#[tokio::test]
async fn a_message_above_the_range_is_not_proof_of_no_gap() {
    let s = setup(
        vec![page(9, None, vec![], vec![])],
        Hist {
            ids: vec!["m5", "m4"],
            gate: None,
        },
        None,
    );
    s.cache.sync_now().await.unwrap();
    s.cache.load_head("c", 50).await.unwrap(); // covers m4..m5, complete
    s.cache
        .live_event("message.new", &msg("m7", "c", 12, "bob", "live"))
        .await;
    let p = s.cache.cached_messages("c", None, 2).await.unwrap();
    assert_eq!(ids(&p.messages), vec!["m7", "m5"]);
    assert!(p.needs_network, "m6 may be missing");
}

/// The start is known: a full page reaching below the range bottom (a tombstone history
/// never lists) is still complete.
#[tokio::test]
async fn a_known_start_makes_any_page_complete() {
    let s = setup(
        vec![page(9, None, vec![], vec![])],
        Hist {
            ids: vec!["m2"],
            gate: None,
        },
        None,
    );
    s.cache.sync_now().await.unwrap();
    s.cache
        .live_event(
            "message.delete",
            &json!({ "id": "m1", "channel_id": "c", "seq": 5 }),
        )
        .await;
    s.cache.load_head("c", 50).await.unwrap(); // short: complete_to_start
    let p = s.cache.cached_messages("c", None, 2).await.unwrap();
    assert_eq!(ids(&p.messages), vec!["m2", "m1"]);
    assert!(!p.needs_network);
}

/// The server caps a page at 100: a full capped page isn't "short, so that was the start".
#[tokio::test]
async fn a_capped_page_is_not_the_start() {
    let many: Vec<&'static str> = (0..200)
        .map(|i| &*Box::leak(format!("m{:03}", 199 - i).into_boxed_str()))
        .collect();
    let s = setup(
        vec![page(9, None, vec![], vec![])],
        Hist {
            ids: many,
            gate: None,
        },
        None,
    );
    s.cache.sync_now().await.unwrap();
    s.cache.load_head("c", 500).await.unwrap();
    let p = s
        .cache
        .cached_messages("c", Some("m100"), 50)
        .await
        .unwrap();
    assert!(p.messages.is_empty());
    assert!(
        p.needs_network,
        "the history below the capped page was taken as absent"
    );
}

/// History for a channel not synced yet is dropped, so it must not leave coverage behind.
#[tokio::test]
async fn history_before_the_channel_arrives_leaves_no_coverage() {
    let s = setup(
        vec![page(9, None, vec![], vec![])],
        Hist {
            ids: vec!["m2", "m1"],
            gate: None,
        },
        None,
    );
    s.cache.load_head("c", 50).await.unwrap(); // before any sync
    s.cache.sync_now().await.unwrap();
    let p = s.cache.cached_messages("c", None, 50).await.unwrap();
    assert!(p.needs_network, "an empty channel claimed complete");
}

/// A cancelled sync doesn't leave the state saying "syncing".
#[tokio::test]
async fn a_cancelled_sync_stops_saying_syncing() {
    let gate = Arc::new(Notify::new());
    let s = setup(
        vec![page(9, None, vec![], vec![])],
        no_history(),
        Some(gate),
    );
    let run = tokio::spawn({
        let c = s.cache.clone();
        async move { c.sync_now().await }
    });
    let server = s.server.clone();
    eventually("the run", move || server.calls.load(Ordering::SeqCst) == 1).await;
    assert!(s.cache.state().borrow().syncing);
    run.abort();
    let _ = run.await;
    assert!(!s.cache.state().borrow().syncing);
}

/// Page one commits; page two fails. Page one's changes are still announced.
#[tokio::test]
async fn committed_pages_are_announced_even_if_a_later_one_fails() {
    struct TwoPages(AtomicUsize);
    #[async_trait::async_trait]
    impl Fetch for TwoPages {
        async fn page(&self, _since: &str) -> Result<Page, crate::Error> {
            match self.0.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    let mut p = page(5, None, vec![msg("m1", "c", 4, "bob", "a")], vec![]);
                    p["more"] = json!(true);
                    Ok(Page::Rows(p))
                }
                _ => Err(crate::Error::UnexpectedResponse),
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let slot: Arc<dyn KeySlot> = Arc::new(InMemoryKeySlot::default());
    let db = match store::open(dir.path(), Kind::Cache, "s", &KeyStore::new(slot)).unwrap() {
        Opened::Ready { db, .. } => db,
        other => panic!("{other:?}"),
    };
    let cache = Cache::new(
        db,
        ME.into(),
        Arc::new(TwoPages(AtomicUsize::new(0))),
        Arc::new(no_history()),
    );
    let mut events = cache.events();
    assert!(cache.sync_now().await.is_err());
    assert_eq!(
        events.try_recv().ok(),
        Some(CacheEvent::Channels(vec!["c".into()]))
    );
}

/// A sync that only grows a range (no row changed) still tells the UI: a page may no
/// longer need the network.
#[tokio::test]
async fn a_range_that_grows_is_announced() {
    let s = setup(
        vec![
            page(9, None, vec![], vec![]),
            page(12, None, vec![msg("m1", "c", 11, "bob", "first")], vec![]),
        ],
        no_history(),
        None,
    );
    s.cache.sync_now().await.unwrap();
    s.cache.load_head("c", 50).await.unwrap(); // empty and complete
    s.cache
        .live_event("message.new", &msg("m1", "c", 11, "bob", "first"))
        .await;
    let mut events = s.cache.events();
    s.cache.sync_now().await.unwrap(); // m1 again: no row change, but the range settles
    let mut got = vec![];
    while let Ok(e) = events.try_recv() {
        got.push(e);
    }
    assert!(
        got.contains(&CacheEvent::Channels(vec!["c".into()])),
        "{got:?}"
    );
    assert!(
        !s.cache
            .cached_messages("c", None, 50)
            .await
            .unwrap()
            .needs_network
    );
}

// ---- Review round 2 ----

/// Paging from a live message above the range: the gap below it is unverified.
#[tokio::test]
async fn paging_from_above_the_range_needs_the_network() {
    let s = setup(
        vec![page(9, None, vec![], vec![])],
        Hist {
            ids: vec!["m5", "m4"],
            gate: None,
        },
        None,
    );
    s.cache.sync_now().await.unwrap();
    s.cache.load_head("c", 50).await.unwrap();
    s.cache
        .live_event("message.new", &msg("m7", "c", 12, "bob", "live"))
        .await;
    let p = s.cache.cached_messages("c", Some("m7"), 2).await.unwrap();
    assert_eq!(ids(&p.messages), vec!["m5", "m4"]);
    assert!(
        p.needs_network,
        "m6 may be missing between m7 and the range"
    );
}

/// A tombstone above the newest surviving message: history never lists it, and it hides
/// nothing, so the head page is complete after one load.
#[tokio::test]
async fn a_tombstone_above_the_head_does_not_keep_it_incomplete() {
    let mut tomb = msg("m2", "c", 8, "bob", "");
    tomb["deleted_at"] = json!("2026-09-25T11:00:00Z");
    let s = setup(
        vec![page(9, None, vec![tomb], vec![])],
        Hist {
            ids: vec!["m1"],
            gate: None,
        },
        None,
    );
    s.cache.sync_now().await.unwrap();
    s.cache.load_head("c", 50).await.unwrap();
    let p = s.cache.cached_messages("c", None, 50).await.unwrap();
    assert_eq!(ids(&p.messages), vec!["m2", "m1"]);
    assert!(!p.needs_network);
}

/// Removed, then a channel row (not the caller's membership) brings the channel back: the
/// fence still rejects history, so no coverage is recorded from it.
#[tokio::test]
async fn history_under_an_active_fence_leaves_no_coverage() {
    let s = setup(
        vec![page(9, None, vec![], vec![])],
        Hist {
            ids: vec!["m2", "m1"],
            gate: None,
        },
        None,
    );
    s.cache.sync_now().await.unwrap();
    s.cache
        .live_event("channel.delete", &json!({ "id": "c", "seq": 20 }))
        .await;
    s.cache
        .live_event(
            "channel.update",
            &json!({ "id": "c", "name": "general", "seq": 21 }),
        )
        .await;
    s.cache.load_head("c", 50).await.unwrap();
    let p = s.cache.cached_messages("c", None, 50).await.unwrap();
    assert!(p.messages.is_empty());
    assert!(p.needs_network, "coverage from rejected rows");
}

/// Cancelled while page two is out: page one's committed changes are still announced.
#[tokio::test]
async fn a_cancelled_run_still_announces_what_it_committed() {
    struct OneThenHang(AtomicUsize);
    #[async_trait::async_trait]
    impl Fetch for OneThenHang {
        async fn page(&self, _since: &str) -> Result<Page, crate::Error> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                let mut p = page(5, None, vec![msg("m1", "c", 4, "bob", "a")], vec![]);
                p["more"] = json!(true);
                return Ok(Page::Rows(p));
            }
            std::future::pending().await
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let slot: Arc<dyn KeySlot> = Arc::new(InMemoryKeySlot::default());
    let db = match store::open(dir.path(), Kind::Cache, "s", &KeyStore::new(slot)).unwrap() {
        Opened::Ready { db, .. } => db,
        other => panic!("{other:?}"),
    };
    let fetch = Arc::new(OneThenHang(AtomicUsize::new(0)));
    let cache = Cache::new(db, ME.into(), fetch.clone(), Arc::new(no_history()));
    let mut events = cache.events();
    let run = tokio::spawn({
        let c = cache.clone();
        async move { c.sync_now().await }
    });
    let f = fetch.clone();
    eventually("page two requested", move || {
        f.0.load(Ordering::SeqCst) == 2
    })
    .await;
    run.abort();
    let _ = run.await;
    assert_eq!(
        events.try_recv().ok(),
        Some(CacheEvent::Channels(vec!["c".into()]))
    );
}

/// A refused token is not "offline" (the app signs in again); an unreachable server is.
#[tokio::test]
async fn only_an_unreachable_server_is_offline() {
    struct Refused;
    #[async_trait::async_trait]
    impl Fetch for Refused {
        async fn page(&self, _since: &str) -> Result<Page, crate::Error> {
            Err(crate::Error::NotAuthenticated)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let slot: Arc<dyn KeySlot> = Arc::new(InMemoryKeySlot::default());
    let db = match store::open(dir.path(), Kind::Cache, "s", &KeyStore::new(slot)).unwrap() {
        Opened::Ready { db, .. } => db,
        other => panic!("{other:?}"),
    };
    let cache = Cache::new(db, ME.into(), Arc::new(Refused), Arc::new(no_history()));
    assert!(cache.sync_now().await.is_err());
    assert!(
        !cache.state().borrow().offline,
        "a refused token read as offline"
    );
}

mod post_http {
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::watch;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cache_http::Http;
    use crate::outbox::{Post, SendFailure};
    use crate::session_store::SessionStore;
    use crate::{AuthState, Session, User};

    async fn http(server: &MockServer) -> (Http, u64) {
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
        let epoch = session.snapshot().await.0.epoch;
        let h = Http {
            http: reqwest::Client::new(),
            base: format!("{}/", server.uri()).parse().unwrap(),
            session,
            transfers: Arc::new(crate::transfer::Transfers::new()),
        };
        (h, epoch)
    }

    async fn answer(
        status: u16,
        body: serde_json::Value,
        headers: &[(&str, &str)],
    ) -> Result<serde_json::Value, SendFailure> {
        let server = MockServer::start().await;
        let mut t = ResponseTemplate::new(status).set_body_json(body);
        for (k, v) in headers {
            t = t.insert_header(*k, *v);
        }
        Mock::given(method("POST"))
            .and(path("/api/v1/channels/c1/messages"))
            .and(body_partial_json(
                json!({ "body": "hi", "client_id": "cid" }),
            ))
            .respond_with(t)
            .mount(&server)
            .await;
        let (h, epoch) = http(&server).await;
        h.send("c1", &super::hi(), "cid", epoch).await
    }

    #[tokio::test]
    async fn the_status_table() {
        assert!(answer(201, json!({ "id": "m1", "client_id": "cid" }), &[])
            .await
            .is_ok());
        assert!(answer(200, json!({ "id": "m1", "client_id": "cid" }), &[])
            .await
            .is_ok());
        assert_eq!(
            answer(429, json!({}), &[("retry-after", "7")]).await,
            Err(SendFailure::Transient {
                retry_after: Some(7)
            })
        );
        for s in [401, 408, 500, 503] {
            assert!(
                matches!(
                    answer(s, json!({}), &[]).await,
                    Err(SendFailure::Transient { .. })
                ),
                "{s}"
            );
        }
        let refused = json!({ "error": { "code": "authz.forbidden", "message": "no" } });
        assert_eq!(
            answer(403, refused, &[]).await,
            Err(SendFailure::Refused {
                code: "authz.forbidden".into()
            })
        );
        assert_eq!(
            answer(409, json!({}), &[]).await,
            Err(SendFailure::Refused {
                code: "http_409".into()
            })
        );
    }

    /// Asked under a session that isn't the current one: nothing is sent.
    #[tokio::test]
    async fn a_stale_session_sends_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({})))
            .expect(0)
            .mount(&server)
            .await;
        let (h, epoch) = http(&server).await;
        assert!(matches!(
            h.send("c1", &super::hi(), "cid", epoch + 1).await,
            Err(SendFailure::Transient { .. })
        ));
    }
}

/// A closed cache takes no more syncs, and its store can be reset right after.
#[tokio::test]
async fn a_closed_cache_releases_its_store() {
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let keys = KeyStore::new(slot.clone() as Arc<dyn KeySlot>);
    let db = match store::open(dir.path(), Kind::Cache, "s", &keys).unwrap() {
        Opened::Ready { db, .. } => db,
        other => panic!("{other:?}"),
    };
    let server = Arc::new(Server {
        pages: Mutex::new(vec![page(9, None, vec![], vec![])]),
        calls: AtomicUsize::new(0),
        since: Mutex::new(vec![]),
        gate: None,
    });
    let cache = Cache::new(db, ME.into(), server.clone(), Arc::new(no_history()));
    cache.schedule_sync(); // pending when the close comes
    cache.close().await;
    tokio::time::sleep(crate::cache::HINT_DEBOUNCE * 2).await;
    assert_eq!(server.calls.load(Ordering::SeqCst), 0, "synced after close");
    assert!(
        store::reset(dir.path(), Kind::Cache, "s", &keys).is_ok(),
        "still open"
    );
}

/// `410 sync.reset`: the synced rows go, the cursor starts over, and a sync from 0 refills
/// the cache with what the server has now.
#[tokio::test]
async fn a_reset_clears_the_rows_and_syncs_from_zero() {
    let s = setup(
        vec![
            page(5, None, vec![msg("m1", "c", 4, "bob", "before")], vec![]),
            json!("RESET"),
            page(7, None, vec![msg("m2", "c", 6, "bob", "after")], vec![]),
        ],
        no_history(),
        None,
    );
    s.cache.sync_now().await.unwrap();
    let mut events = s.cache.events();
    s.cache.sync_now().await.unwrap();
    assert_eq!(
        *s.server.since.lock().unwrap(),
        vec!["0", "5", "0"],
        "no sync from 0 after the reset"
    );
    let bodies: Vec<Value> = s
        .cache
        .cached_messages("c", None, 10)
        .await
        .unwrap()
        .messages
        .iter()
        .map(|m| m["body"].clone())
        .collect();
    assert_eq!(
        bodies,
        vec![json!("after")],
        "a row from before the reset stayed"
    );
    let mut got = vec![];
    while let Ok(e) = events.try_recv() {
        got.push(e);
    }
    assert!(got.contains(&CacheEvent::Reset), "{got:?}");
}

/// A history page asked for before a reset doesn't land after it (the channel is back by
/// then, so only the generation tells).
#[tokio::test]
async fn a_history_page_from_before_a_reset_is_dropped() {
    let gate = Arc::new(Notify::new());
    let s = setup(
        vec![
            page(5, None, vec![], vec![]),
            json!("RESET"),
            page(7, None, vec![], vec![]),
        ],
        Hist {
            ids: vec!["m9"],
            gate: Some(gate.clone()),
        },
        None,
    );
    s.cache.sync_now().await.unwrap();
    let load = tokio::spawn({
        let cache = s.cache.clone();
        async move { cache.load_head("c", 10).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await; // the load read the generation
    s.cache.sync_now().await.unwrap();
    gate.notify_one();
    load.await.unwrap().unwrap();
    let page = s.cache.cached_messages("c", None, 10).await.unwrap();
    assert!(page.messages.is_empty(), "{:?}", page.messages);
}

/// A server that answers 410 to `since=0` as well gets no second round (no loop).
#[tokio::test]
async fn a_reset_at_zero_does_not_loop() {
    let s = setup(vec![json!("RESET")], no_history(), None);
    tokio::time::timeout(Duration::from_secs(5), s.cache.sync_now())
        .await
        .expect("looped on the reset")
        .unwrap();
    assert_eq!(s.server.calls.load(Ordering::SeqCst), 1);
}

fn hi() -> crate::outbox::Outgoing {
    crate::outbox::Outgoing {
        body: "hi".into(),
        ..Default::default()
    }
}

/// A reply's POST names its target; a plain message's has no `reply_to_id` at all.
#[test]
fn the_send_body_names_the_reply_target_only_for_a_reply() {
    use crate::cache_http::send_body;
    let mut m = hi();
    assert!(send_body(&m, "cid").get("reply_to_id").is_none());
    m.reply_to_id = Some("q".into());
    assert_eq!(send_body(&m, "cid")["reply_to_id"], "q");
    assert_eq!(send_body(&m, "cid")["client_id"], "cid");
    assert!(send_body(&m, "cid").get("attachments").is_none());
    m.attachments = vec!["f2".into(), "f1".into()];
    assert_eq!(send_body(&m, "cid")["attachments"], serde_json::json!(["f2", "f1"]));
}
