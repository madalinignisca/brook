//! The outbox (plan C4, spec §5 and §9 "Outbox"): durable before pending, one message per
//! send whatever is lost on the way, order kept, failures scoped to their channel, the
//! status table row by row, and paused while signed out.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{watch, Notify};

use crate::cache::{Cache, History};
use crate::outbox::{Deleted, Outbox, OutboxError, Outgoing, PendingState, Post, SendFailure};
use crate::store::{self, Db, Kind, Opened};
use crate::sync::{Fetch, Page};
use crate::{InMemoryKeySlot, KeySlot, KeyStore};

const ME: &str = "me";

/// What the fake server does with the next send.
#[derive(Clone)]
enum Answer {
    /// Store it (once per client_id) and answer with it.
    Ok,
    /// Store it, but the answer is lost on the way.
    Lost,
    Fail(SendFailure),
    /// Answer with somebody else's client_id (a bug).
    WrongEcho,
    /// Hold the answer until released, then store and answer.
    Held(Arc<Notify>),
}

#[derive(Default)]
struct Server {
    script: Mutex<VecDeque<Answer>>,
    /// Answers for one message body (taken before the shared script).
    by_body: Mutex<HashMap<String, VecDeque<Answer>>>,
    stored: Mutex<HashMap<String, Value>>,
    /// Every send, in order: (channel, client_id, body).
    sends: Mutex<Vec<(String, String, String)>>,
    /// Every send's reply target, in order: (client_id, reply_to_id).
    replies: Mutex<Vec<(String, Option<String>)>>,
}

impl Server {
    fn script(&self, answers: impl IntoIterator<Item = Answer>) {
        self.script.lock().unwrap().extend(answers);
    }
    fn script_for(&self, body: &str, answers: impl IntoIterator<Item = Answer>) {
        self.by_body
            .lock()
            .unwrap()
            .entry(body.into())
            .or_default()
            .extend(answers);
    }
    fn sends(&self) -> Vec<(String, String, String)> {
        self.sends.lock().unwrap().clone()
    }
    fn stored_bodies(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .stored
            .lock()
            .unwrap()
            .values()
            .map(|m| m["body"].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    }
    fn store(&self, channel: &str, body: &str, client_id: &str) -> Value {
        // Like the server: the id is parsed as a UUID and echoed canonical (lowercase).
        let client_id = &client_id.to_ascii_lowercase();
        let mut stored = self.stored.lock().unwrap();
        let n = stored.len() + 1;
        stored
            .entry(client_id.to_string())
            .or_insert_with(|| {
                json!({ "id": format!("m{n:03}"), "channel_id": channel, "author_id": ME,
                        "body": body, "created_at": "2026-09-25T10:00:00Z",
                        "seq": 100 + n as i64, "client_id": client_id })
            })
            .clone()
    }
}

#[async_trait::async_trait]
impl Post for Server {
    async fn send(
        &self,
        channel: &str,
        msg: &Outgoing,
        client_id: &str,
        _epoch: u64,
    ) -> Result<Value, SendFailure> {
        let body = msg.body.as_str();
        self.replies
            .lock()
            .unwrap()
            .push((client_id.into(), msg.reply_to_id.clone()));
        self.sends
            .lock()
            .unwrap()
            .push((channel.into(), client_id.into(), body.into()));
        let scripted = self
            .by_body
            .lock()
            .unwrap()
            .get_mut(body)
            .and_then(VecDeque::pop_front);
        let answer = scripted
            .or_else(|| self.script.lock().unwrap().pop_front())
            .unwrap_or(Answer::Ok);
        match answer {
            Answer::Ok => Ok(self.store(channel, body, client_id)),
            Answer::Lost => {
                self.store(channel, body, client_id);
                Err(SendFailure::Transient {
                    retry_after: Some(0),
                })
            }
            Answer::Fail(f) => Err(f),
            Answer::WrongEcho => {
                let mut m = self.store(channel, body, client_id);
                m["client_id"] = json!("someone-else");
                Ok(m)
            }
            Answer::Held(gate) => {
                gate.notified().await;
                Ok(self.store(channel, body, client_id))
            }
        }
    }
}

struct NoSync;
#[async_trait::async_trait]
impl Fetch for NoSync {
    async fn page(&self, _: &str) -> Result<Page, crate::Error> {
        Err(crate::Error::Timeout)
    }
}
#[async_trait::async_trait]
impl History for NoSync {
    async fn page(&self, _: &str, _: Option<&str>, _: usize) -> Result<Vec<Value>, crate::Error> {
        Ok(vec![])
    }
}

struct Setup {
    outbox: Arc<Outbox>,
    server: Arc<Server>,
    cache: Arc<Cache>,
    session: watch::Sender<Option<u64>>,
    outbox_dir: tempfile::TempDir,
    slot: Arc<InMemoryKeySlot>,
    _cache_dir: tempfile::TempDir,
}

/// Open an outbox over `db` with `post` (whose sends it makes) and the test server's uploads,
/// its snapshots under `dir`.
async fn open_outbox(
    db: Db,
    cache: Arc<Cache>,
    post: Arc<dyn Post>,
    rx: watch::Receiver<Option<u64>>,
    dir: &std::path::Path,
) -> Result<Arc<Outbox>, crate::store::StoreError> {
    let outbox = Outbox::open(
        db,
        cache,
        post,
        Arc::new(NoUploads),
        Arc::new(crate::transfer::Transfers::new()),
        rx,
        dir,
    )
    .await?;
    outbox.set_chunk_for_tests(8);
    Ok(outbox)
}

/// For the text-only tests: an upload is never asked for.
struct NoUploads;

#[async_trait::async_trait]
impl crate::outbox::Upload for NoUploads {
    async fn upload(
        &self,
        _: crate::transfer::TransferId,
        _: &Arc<crate::transfer::Flags>,
        _: &str,
        _: &crate::outbox::FileRow,
        _: &crate::snapshot::SnapshotSource,
        _: u64,
    ) -> Result<crate::transfer::FileInfo, crate::Error> {
        panic!("a text message uploaded a file")
    }
}

fn open_db(dir: &std::path::Path, kind: Kind, slot: &Arc<InMemoryKeySlot>) -> Db {
    let keys = KeyStore::new(slot.clone() as Arc<dyn KeySlot>);
    match store::open(dir, kind, "s", &keys).unwrap() {
        Opened::Ready { db, .. } => db,
        other => panic!("{other:?}"),
    }
}

async fn setup() -> Setup {
    let slot = Arc::new(InMemoryKeySlot::default());
    let cache_dir = tempfile::tempdir().unwrap();
    let outbox_dir = tempfile::tempdir().unwrap();
    let cache = Cache::new(
        open_db(cache_dir.path(), Kind::Cache, &slot),
        ME.into(),
        Arc::new(NoSync),
        Arc::new(NoSync),
    );
    // The channels exist in the cache (acks apply only to a present channel).
    for ch in ["c1", "c2"] {
        cache
            .live_event("channel.update", &json!({ "id": ch, "name": ch, "seq": 1 }))
            .await;
    }
    let server = Arc::new(Server::default());
    let (session, rx) = watch::channel(Some(1));
    let outbox = open_outbox(
        open_db(outbox_dir.path(), Kind::Outbox, &slot),
        cache.clone(),
        server.clone(),
        rx,
        outbox_dir.path(),
    )
    .await
    .unwrap();
    Setup {
        outbox,
        server,
        cache,
        session,
        outbox_dir,
        slot,
        _cache_dir: cache_dir,
    }
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

async fn drained(s: &Setup) {
    for _ in 0..500 {
        if s.outbox.unsent_count().await.unwrap() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the outbox never drained: {:?}",
        s.outbox.pending("c1").await.unwrap()
    );
}

async fn cached_bodies(s: &Setup, ch: &str) -> Vec<String> {
    let mut v: Vec<String> = s
        .cache
        .cached_messages(ch, None, 100)
        .await
        .unwrap()
        .messages
        .iter()
        .map(|m| m["body"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn a_queued_message_is_durable_before_enqueue_returns() {
    let s = setup().await;
    s.session.send_replace(None); // signed out: nothing is sent
    let id = s.outbox.enqueue("c1", "hello", None, None).await.unwrap();
    // Reopen the store as a restart would: the row is there.
    let outbox = s.outbox;
    outbox.close().await;
    let db = open_db(s.outbox_dir.path(), Kind::Outbox, &s.slot);
    let rows: i64 = db
        .call(move |c| {
            c.query_row(
                "SELECT count(*) FROM outbox WHERE client_id = ?1",
                [&id],
                |r| r.get(0),
            )
        })
        .await
        .unwrap_or(0);
    assert_eq!(rows, 1);
}

/// The answer is lost; the resend (same client_id) gets the stored message: one message.
#[tokio::test]
async fn a_lost_answer_and_a_resend_make_one_message() {
    let s = setup().await;
    s.server.script([Answer::Lost, Answer::Ok]);
    s.outbox.enqueue("c1", "once", None, None).await.unwrap();
    drained(&s).await;
    let sends = s.server.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].1, sends[1].1, "the resend used another client_id");
    assert_eq!(s.server.stored_bodies(), vec!["once"]);
    assert_eq!(cached_bodies(&s, "c1").await, vec!["once"]);
}

#[tokio::test]
async fn messages_go_in_the_order_written() {
    let s = setup().await;
    s.session.send_replace(None);
    for body in ["one", "two", "three", "four"] {
        s.outbox.enqueue("c1", body, None, None).await.unwrap();
    }
    s.session.send_replace(Some(1));
    drained(&s).await;
    let order: Vec<String> = s.server.sends().into_iter().map(|x| x.2).collect();
    assert_eq!(order, vec!["one", "two", "three", "four"]);
}

/// A crash while `sending`: at the next start the row goes out again with the same id.
#[tokio::test]
async fn a_row_left_sending_by_a_crash_is_resent_with_its_id() {
    let s = setup().await;
    s.session.send_replace(None);
    let id = s
        .outbox
        .enqueue("c1", "mid-flight", None, None)
        .await
        .unwrap();
    let outbox = s.outbox;
    outbox.close().await;
    let db = open_db(s.outbox_dir.path(), Kind::Outbox, &s.slot);
    db.call(|c| c.execute("UPDATE outbox SET state = 'sending'", []))
        .await
        .unwrap();
    let (session, rx) = watch::channel(None);
    let outbox = open_outbox(
        db,
        s.cache.clone(),
        s.server.clone(),
        rx,
        s.outbox_dir.path(),
    )
    .await
    .unwrap();
    // Before anything is sent again, it reads as waiting, not as mid-send.
    assert_eq!(
        outbox.pending("c1").await.unwrap()[0].state,
        PendingState::Pending
    );
    session.send_replace(Some(1));
    outbox.resume().await.unwrap();
    let server = s.server.clone();
    eventually("the resend", move || !server.sends().is_empty()).await;
    assert_eq!(s.server.sends()[0].1, id);
    drop(session);
}

#[tokio::test]
async fn a_failed_message_blocks_only_its_own_channel() {
    let s = setup().await;
    s.session.send_replace(None);
    s.server.script_for(
        "refused",
        [Answer::Fail(SendFailure::Refused {
            code: "authz.forbidden".into(),
        })],
    );
    s.outbox.enqueue("c1", "refused", None, None).await.unwrap();
    s.outbox
        .enqueue("c1", "behind it", None, None)
        .await
        .unwrap();
    s.outbox
        .enqueue("c2", "elsewhere", None, None)
        .await
        .unwrap();
    s.session.send_replace(Some(1));
    let server = s.server.clone();
    eventually("c2's send", move || {
        server.sends().iter().any(|x| x.2 == "elsewhere")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let c1 = s.outbox.pending("c1").await.unwrap();
    assert_eq!(c1.len(), 2, "a later message overtook the failed one");
    assert_eq!(
        c1[0].state,
        PendingState::Failed {
            code: "authz.forbidden".into()
        }
    );
    assert_eq!(c1[1].state, PendingState::Pending);
    assert!(!s.server.sends().iter().any(|x| x.2 == "behind it"));
    // Retry: it goes, then the one behind it.
    s.outbox.retry(&c1[0].client_id).await.unwrap();
    drained(&s).await;
}

/// The status table, row by row: these stay pending (and go out later), never failed.
#[tokio::test]
async fn transient_answers_keep_the_message_pending() {
    for failure in [
        SendFailure::Transient {
            retry_after: Some(0),
        },
        SendFailure::Transient { retry_after: None },
    ] {
        let s = setup().await;
        s.server.script([Answer::Fail(failure.clone()), Answer::Ok]);
        s.outbox
            .enqueue("c1", "eventually", None, None)
            .await
            .unwrap();
        drained(&s).await;
        assert_eq!(s.server.stored_bodies(), vec!["eventually"], "{failure:?}");
    }
}

#[tokio::test]
async fn a_refusal_fails_the_message_with_the_servers_code() {
    for code in [
        "authz.forbidden",
        "not_found",
        "validation_error",
        "file.not_attachable",
    ] {
        let s = setup().await;
        s.server
            .script([Answer::Fail(SendFailure::Refused { code: code.into() })]);
        s.outbox.enqueue("c1", "no", None, None).await.unwrap();
        let o = s.outbox.clone();
        let mut state = None;
        for _ in 0..500 {
            state = o
                .pending("c1")
                .await
                .unwrap()
                .first()
                .map(|m| m.state.clone());
            if matches!(state, Some(PendingState::Failed { .. })) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(state, Some(PendingState::Failed { code: code.into() }));
    }
}

/// An answer that echoes another message's client_id is a bug: the row fails, never resent.
#[tokio::test]
async fn a_wrong_echo_fails_the_row_and_never_resends() {
    let s = setup().await;
    s.server.script([Answer::WrongEcho]);
    s.outbox
        .enqueue("c1", "who am i", None, None)
        .await
        .unwrap();
    let o = s.outbox.clone();
    for _ in 0..500 {
        if matches!(
            o.pending("c1")
                .await
                .unwrap()
                .first()
                .map(|m| m.state.clone()),
            Some(PendingState::Failed { .. })
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.server.sends().len(), 1, "a mismatched row was resent");
    assert_eq!(
        s.outbox.pending("c1").await.unwrap()[0].state,
        PendingState::Failed {
            code: "outbox.echo_mismatch".into()
        }
    );
}

/// Delete while the send is in flight and the server accepts it: it's sent, and stays.
#[tokio::test]
async fn delete_during_an_accepted_send_reports_it_sent() {
    let s = setup().await;
    let gate = Arc::new(Notify::new());
    s.server.script([Answer::Held(gate.clone())]);
    let id = s
        .outbox
        .enqueue("c1", "too late", None, None)
        .await
        .unwrap();
    let server = s.server.clone();
    eventually("the send in flight", move || server.sends().len() == 1).await;
    let delete = tokio::spawn({
        let (o, id) = (s.outbox.clone(), id.clone());
        async move { o.delete_pending(&id).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.notify_one();
    assert_eq!(delete.await.unwrap().unwrap(), Deleted::AlreadySent);
    assert_eq!(cached_bodies(&s, "c1").await, vec!["too late"]);
}

#[tokio::test]
async fn delete_before_sending_removes_it() {
    let s = setup().await;
    s.session.send_replace(None);
    let id = s
        .outbox
        .enqueue("c1", "never mind", None, None)
        .await
        .unwrap();
    assert_eq!(
        s.outbox.delete_pending(&id).await.unwrap(),
        Deleted::Removed
    );
    s.session.send_replace(Some(1));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(s.server.sends().is_empty());
}

/// Signed out during a backoff sleep: the woken sender doesn't send until the next sign-in.
#[tokio::test]
async fn a_sign_out_pauses_the_sender_mid_backoff() {
    let s = setup().await;
    s.server.script([Answer::Fail(SendFailure::Transient {
        retry_after: Some(1),
    })]);
    s.outbox.enqueue("c1", "later", None, None).await.unwrap();
    let server = s.server.clone();
    eventually("the first attempt", move || server.sends().len() == 1).await;
    s.session.send_replace(None); // a plain sign-out (no data removed)
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(s.server.sends().len(), 1, "sent while signed out");
    assert_eq!(s.outbox.unsent_count().await.unwrap(), 1);
    s.session.send_replace(Some(2));
    drained(&s).await;
}

#[tokio::test]
async fn unsent_messages_are_counted_for_the_sign_out_warning() {
    let s = setup().await;
    s.session.send_replace(None);
    s.outbox.enqueue("c1", "a", None, None).await.unwrap();
    s.outbox.enqueue("c2", "b", None, None).await.unwrap();
    assert_eq!(s.outbox.unsent_count().await.unwrap(), 2);
}

#[test]
fn would_overtake_is_its_own_error() {
    assert_ne!(OutboxError::WouldOvertake, OutboxError::Store);
}

/// The ack is applied to the cache first; if that fails the row stays (it's resent, and the
/// server answers with the stored message): the message is never lost between the two.
#[tokio::test]
async fn a_failed_ack_keeps_the_row() {
    struct Garbled(Arc<Server>);
    #[async_trait::async_trait]
    impl Post for Garbled {
        async fn send(
            &self,
            ch: &str,
            msg: &Outgoing,
            cid: &str,
            e: u64,
        ) -> Result<Value, SendFailure> {
            let mut m = self.0.send(ch, msg, cid, e).await?;
            m.as_object_mut().unwrap().remove("created_at"); // the cache can't take it
            Ok(m)
        }
    }
    let s = setup().await;
    let slot = Arc::new(InMemoryKeySlot::default());
    let dir = tempfile::tempdir().unwrap();
    let (session, rx) = watch::channel(Some(1));
    let outbox = open_outbox(
        open_db(dir.path(), Kind::Outbox, &slot),
        s.cache.clone(),
        Arc::new(Garbled(s.server.clone())),
        rx,
        dir.path(),
    )
    .await
    .unwrap();
    outbox.enqueue("c1", "keep me", None, None).await.unwrap();
    let server = s.server.clone();
    eventually("a send", move || !server.sends().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        outbox.pending("c1").await.unwrap().len(),
        1,
        "the row went before the cache had it"
    );
    drop(session);
}

// ---- Review round 1 ----

async fn trigger(s: &Setup, sql: &'static str) {
    s.outbox
        .db_for_tests()
        .call(move |c| c.execute_batch(sql))
        .await
        .unwrap();
}

const NO_INSERTS: &str = "CREATE TEMP TRIGGER full BEFORE INSERT ON outbox
     BEGIN SELECT RAISE(ABORT, 'disk full'); END;";

/// The outbox can't be written: sent directly, only while signed in, and a failed direct
/// send hands back its client_id so a retry is the same message.
#[tokio::test]
async fn a_direct_send_is_signed_in_and_retryable_as_the_same_message() {
    let s = setup().await;
    trigger(&s, NO_INSERTS).await;
    s.session.send_replace(None);
    assert_eq!(
        s.outbox.enqueue("c1", "hi", None, None).await,
        Err(OutboxError::SignedOut)
    );
    s.session.send_replace(Some(1));
    s.server.script([Answer::Lost]);
    let err = s.outbox.enqueue("c1", "hi", None, None).await.unwrap_err();
    let OutboxError::NotSent { client_id, .. } = err else {
        panic!("{err:?}")
    };
    s.outbox
        .enqueue("c1", "hi", None, Some(client_id))
        .await
        .unwrap();
    assert_eq!(
        s.server.stored_bodies(),
        vec!["hi"],
        "the retry made a second message"
    );
}

#[tokio::test]
async fn a_direct_send_never_overtakes_a_queued_message() {
    let s = setup().await;
    s.session.send_replace(None);
    s.outbox.enqueue("c1", "first", None, None).await.unwrap();
    trigger(&s, NO_INSERTS).await;
    s.session.send_replace(Some(1));
    // The queued one may go meanwhile; either way a direct send only runs with nothing
    // left queued, so it can't overtake.
    match s.outbox.enqueue("c1", "second", None, None).await {
        Err(OutboxError::WouldOvertake) => {}
        Ok(_) => assert_eq!(
            s.server
                .sends()
                .iter()
                .map(|x| x.2.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        ),
        other => panic!("{other:?}"),
    }
}

/// A refusal the store can't record doesn't turn into a tight re-send loop.
#[tokio::test]
async fn an_outcome_the_store_cant_record_backs_off() {
    let s = setup().await;
    trigger(
        &s,
        "CREATE TEMP TRIGGER nofail BEFORE UPDATE ON outbox WHEN NEW.state = 'failed'
         BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .await;
    for _ in 0..50 {
        s.server
            .script([Answer::Fail(SendFailure::Refused { code: "x".into() })]);
    }
    s.outbox.enqueue("c1", "stuck", None, None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let n = s.server.sends().len();
    assert!(n <= 2, "re-sent {n} times in 1.5 s");
}

/// Signed out while the send was out: its answer isn't applied, the row stays.
#[tokio::test]
async fn an_answer_after_a_sign_out_is_not_applied() {
    let s = setup().await;
    let gate = Arc::new(Notify::new());
    s.server.script([Answer::Held(gate.clone())]);
    s.outbox
        .enqueue("c1", "in flight", None, None)
        .await
        .unwrap();
    let server = s.server.clone();
    eventually("the send", move || server.sends().len() == 1).await;
    s.session.send_replace(None);
    gate.notify_one();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        s.outbox.unsent_count().await.unwrap(),
        1,
        "the row went after sign-out"
    );
    assert!(
        cached_bodies(&s, "c1").await.is_empty(),
        "applied after sign-out"
    );
}

/// The server took it but the cache couldn't: `accepted`, and Delete says it's sent.
#[tokio::test]
async fn an_accepted_message_is_never_deleted_as_unsent() {
    struct Garbled(Arc<Server>);
    #[async_trait::async_trait]
    impl Post for Garbled {
        async fn send(
            &self,
            ch: &str,
            msg: &Outgoing,
            cid: &str,
            e: u64,
        ) -> Result<Value, SendFailure> {
            let mut m = self.0.send(ch, msg, cid, e).await?;
            m.as_object_mut().unwrap().remove("created_at");
            Ok(m)
        }
    }
    let s = setup().await;
    let slot = Arc::new(InMemoryKeySlot::default());
    let dir = tempfile::tempdir().unwrap();
    let (_session, rx) = watch::channel(Some(1));
    let outbox = open_outbox(
        open_db(dir.path(), Kind::Outbox, &slot),
        s.cache.clone(),
        Arc::new(Garbled(s.server.clone())),
        rx,
        dir.path(),
    )
    .await
    .unwrap();
    let id = outbox
        .enqueue("c1", "sent really", None, None)
        .await
        .unwrap();
    let o = outbox.clone();
    for _ in 0..500 {
        if o.pending("c1")
            .await
            .unwrap()
            .first()
            .map(|m| m.state.clone())
            == Some(PendingState::Accepted)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        outbox.pending("c1").await.unwrap()[0].state,
        PendingState::Accepted
    );
    assert_eq!(outbox.unsent_count().await.unwrap(), 0, "counted as unsent");
    assert_eq!(
        outbox.delete_pending(&id).await.unwrap(),
        Deleted::AlreadySent
    );
}

#[tokio::test]
async fn a_closed_outbox_takes_nothing() {
    let s = setup().await;
    let extra = s.outbox.clone(); // someone still holds it
    s.outbox.close().await;
    assert_eq!(
        extra.enqueue("c1", "after", None, None).await,
        Err(OutboxError::Closed)
    );
    assert_eq!(extra.retry("x").await, Err(OutboxError::Closed));
}

/// A 401 is transient: it's retried (the refresh loop renews the token meanwhile).
#[tokio::test]
async fn a_refused_token_is_retried() {
    let s = setup().await;
    s.server.script([Answer::Fail(SendFailure::Transient {
        retry_after: Some(0),
    })]);
    s.outbox
        .enqueue("c1", "after refresh", None, None)
        .await
        .unwrap();
    drained(&s).await;
}

// ---- Review round 2 ----

/// A server saying "retry now" while the store can't record "pending": still no tight loop.
#[tokio::test]
async fn retry_after_zero_with_a_failing_store_is_still_bounded() {
    let s = setup().await;
    trigger(
        &s,
        "CREATE TEMP TRIGGER nopending BEFORE UPDATE ON outbox WHEN NEW.state = 'pending'
         BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
    )
    .await;
    for _ in 0..50 {
        s.server.script([Answer::Fail(SendFailure::Transient {
            retry_after: Some(0),
        })]);
    }
    s.outbox.enqueue("c1", "busy", None, None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let n = s.server.sends().len();
    assert!(n <= 2, "re-sent {n} times in 1.5 s");
}

/// A post that the cache can never take: accepted, one more try, then the row goes (the
/// server has it; /sync brings it) instead of being resent forever. A refusal after
/// acceptance never turns it into "failed".
#[tokio::test]
async fn an_accepted_row_is_never_resent_forever_nor_failed() {
    struct Garbled(Arc<Server>);
    #[async_trait::async_trait]
    impl Post for Garbled {
        async fn send(
            &self,
            ch: &str,
            msg: &Outgoing,
            cid: &str,
            e: u64,
        ) -> Result<Value, SendFailure> {
            let mut m = self.0.send(ch, msg, cid, e).await?;
            m.as_object_mut().unwrap().remove("created_at");
            Ok(m)
        }
    }
    for then in [
        Answer::Ok,
        Answer::Fail(SendFailure::Refused {
            code: "authz.forbidden".into(),
        }),
    ] {
        let s = setup().await;
        let slot = Arc::new(InMemoryKeySlot::default());
        let dir = tempfile::tempdir().unwrap();
        let (_session, rx) = watch::channel(Some(1));
        s.server.script([Answer::Ok, then]);
        let outbox = open_outbox(
            open_db(dir.path(), Kind::Outbox, &slot),
            s.cache.clone(),
            Arc::new(Garbled(s.server.clone())),
            rx,
            dir.path(),
        )
        .await
        .unwrap();
        outbox.enqueue("c1", "sent", None, None).await.unwrap();
        let o = outbox.clone();
        let mut gone = false;
        for _ in 0..600 {
            let rows = o.pending("c1").await.unwrap();
            assert!(
                !rows
                    .iter()
                    .any(|m| matches!(m.state, PendingState::Failed { .. })),
                "an accepted message was marked failed"
            );
            if rows.is_empty() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(gone, "the accepted row never went");
        assert!(s.server.sends().len() <= 2);
    }
}

#[tokio::test]
async fn an_id_queued_for_another_channel_is_refused() {
    let s = setup().await;
    s.session.send_replace(None);
    let id = s.outbox.enqueue("c1", "here", None, None).await.unwrap();
    assert_eq!(
        s.outbox
            .enqueue("c2", "there", None, Some(id.clone()))
            .await,
        Err(OutboxError::IdInUse)
    );
    // The same channel again is the same message (a retry), not an error.
    assert_eq!(
        s.outbox.enqueue("c1", "here", None, Some(id.clone())).await,
        Ok(id)
    );
}

/// "Retry now" from the server still waits at least a second between sends.
#[tokio::test]
async fn retry_after_zero_waits_at_least_a_second() {
    let s = setup().await;
    for _ in 0..50 {
        s.server.script([Answer::Fail(SendFailure::Transient {
            retry_after: Some(0),
        })]);
    }
    s.outbox.enqueue("c1", "busy", None, None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let n = s.server.sends().len();
    assert!(n <= 2, "re-sent {n} times in 1.5 s");
}

/// The session's source is gone (the client was dropped): the sender stops, even though the
/// channel's last value still says "signed in".
#[tokio::test]
async fn a_dropped_session_source_stops_the_sender() {
    let s = setup().await;
    s.server.script([Answer::Fail(SendFailure::Transient {
        retry_after: Some(1),
    })]);
    s.outbox.enqueue("c1", "orphan", None, None).await.unwrap();
    let server = s.server.clone();
    eventually("the first attempt", move || server.sends().len() == 1).await;
    drop(s.session); // last value: Some(1)
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        s.server.sends().len(),
        1,
        "sent after its session source was gone"
    );
}

const UPPER: &str = "0190A000-0000-7000-8000-00000000ABCD";
const LOWER: &str = "0190a000-0000-7000-8000-00000000abcd";

/// Swift's `UUID().uuidString` is uppercase and the server echoes lowercase: the id is
/// stored canonical, so the echo matches and the message goes out and clears.
#[tokio::test]
async fn an_uppercase_client_id_is_sent_and_cleared() {
    let s = setup().await;
    let id = s
        .outbox
        .enqueue("c1", "shout", None, Some(UPPER.into()))
        .await
        .unwrap();
    assert_eq!(id, LOWER, "the id wasn't made canonical");
    drained(&s).await;
    assert_eq!(cached_bodies(&s, "c1").await, vec!["shout"]);
}

/// Retry and delete find the row by the form the caller kept.
#[tokio::test]
async fn retry_and_delete_accept_the_callers_form_of_the_id() {
    let s = setup().await;
    s.server
        .script([Answer::Fail(SendFailure::Refused { code: "x".into() })]);
    s.outbox
        .enqueue("c1", "again", None, Some(UPPER.into()))
        .await
        .unwrap();
    let outbox = s.outbox.clone();
    for _ in 0..500 {
        let p = outbox.pending("c1").await.unwrap();
        if matches!(
            p.first().map(|m| &m.state),
            Some(crate::outbox::PendingState::Failed { .. })
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    s.outbox.retry(UPPER).await.unwrap();
    drained(&s).await;
    s.session.send_replace(None);
    s.outbox
        .enqueue("c1", "never", None, Some(UPPER.replace("ABCD", "ABCE")))
        .await
        .unwrap();
    assert_eq!(
        s.outbox
            .delete_pending(&UPPER.replace("ABCD", "ABCE"))
            .await
            .unwrap(),
        crate::outbox::Deleted::Removed
    );
}

#[tokio::test]
async fn a_client_id_that_isnt_a_uuid_is_refused() {
    let s = setup().await;
    for bad in [
        "hello",
        "0190a000-0000-7000-8000-00000000abcz",
        "0190a0000000-7000-8000-00000000abcd-",
    ] {
        assert_eq!(
            s.outbox.enqueue("c1", "x", None, Some(bad.into())).await,
            Err(OutboxError::BadId),
            "{bad} was accepted"
        );
    }
    assert!(s
        .outbox
        .enqueue(
            "c1",
            "x",
            None,
            Some("0190A00000007000800000000000ABCD".into())
        )
        .await
        .is_ok_and(|id| id == LOWER));
}

// ---- Queued replies (#111) ----

const Q: &str = "0190a000-0000-7000-8000-0000000000aa";
const R: &str = "0190a000-0000-7000-8000-0000000000bb";
const X: &str = "0190a000-0000-7000-8000-0000000000cc";

fn reply(id: &str) -> Option<String> {
    Some(id.to_string())
}

async fn failed(s: &Setup) {
    for _ in 0..500 {
        let p = s.outbox.pending("c1").await.unwrap();
        if matches!(
            p.first().map(|m| &m.state),
            Some(PendingState::Failed { .. })
        ) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the row never failed");
}

/// The pending row shows what it replies to.
#[tokio::test]
async fn a_queued_reply_shows_its_target() {
    let s = setup().await;
    s.session.send_replace(None);
    s.outbox.enqueue("c1", "yes", reply(Q), None).await.unwrap();
    let p = s.outbox.pending("c1").await.unwrap();
    assert_eq!(p[0].reply_to_id.as_deref(), Some(Q));
}

/// The first send and the resend after a lost answer both carry the target: the server
/// keeps the first POST per client_id, so a first send without it couldn't be mended.
#[tokio::test]
async fn every_send_of_a_reply_carries_its_target() {
    let s = setup().await;
    s.server.script([Answer::Lost, Answer::Ok]);
    let id = s.outbox.enqueue("c1", "yes", reply(Q), None).await.unwrap();
    drained(&s).await;
    let sent = s.server.replies.lock().unwrap().clone();
    assert_eq!(sent, vec![(id.clone(), reply(Q)), (id, reply(Q))]);
}

/// A refused reply, retried, still goes as a reply.
#[tokio::test]
async fn a_retried_reply_is_still_a_reply() {
    let s = setup().await;
    s.server
        .script([Answer::Fail(SendFailure::Refused { code: "x".into() })]);
    s.outbox.enqueue("c1", "yes", reply(Q), None).await.unwrap();
    failed(&s).await;
    let id = s.outbox.pending("c1").await.unwrap()[0].client_id.clone();
    s.outbox.retry(&id).await.unwrap();
    drained(&s).await;
    let last = s.server.replies.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last, (id, reply(Q)));
}

/// The outbox can't be written: the direct send is a reply too.
#[tokio::test]
async fn a_direct_send_carries_the_target() {
    let s = setup().await;
    trigger(&s, NO_INSERTS).await;
    s.outbox.enqueue("c1", "yes", reply(Q), None).await.unwrap();
    let sent = s.server.replies.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, reply(Q));
}

/// The same id again with another target: the stored row wins, on the pending row and on
/// the wire. Quoting something else needs a new id.
#[tokio::test]
async fn the_stored_row_wins_on_the_reply_target() {
    for (first, second) in [(None, reply(R)), (reply(Q), reply(R))] {
        let s = setup().await;
        s.session.send_replace(None);
        s.outbox
            .enqueue("c1", "yes", first.clone(), Some(X.into()))
            .await
            .unwrap();
        s.outbox
            .enqueue("c1", "yes", second, Some(X.into()))
            .await
            .unwrap();
        let p = s.outbox.pending("c1").await.unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].reply_to_id, first, "the pending row changed target");
        s.session.send_replace(Some(1));
        drained(&s).await;
        let sent = s.server.replies.lock().unwrap().clone();
        assert_eq!(sent, vec![(X.to_string(), first)], "posted another target");
    }
}

/// The quote is gone: retried as a plain message, in place and under the same id, ahead of
/// a message queued after it.
#[tokio::test]
async fn a_reply_whose_quote_is_gone_is_retried_plain_in_place() {
    let s = setup().await;
    s.server.script([Answer::Fail(SendFailure::Refused {
        code: "message.reply_target_gone".into(),
    })]);
    let id = s
        .outbox
        .enqueue("c1", "first", reply(Q), None)
        .await
        .unwrap();
    failed(&s).await;
    s.outbox.enqueue("c1", "second", None, None).await.unwrap();
    // Only a failed row changes; a pending one keeps its quote.
    s.outbox.retry_without_reply(&id).await.unwrap();
    drained(&s).await;
    let sends = s.server.replies.lock().unwrap().clone();
    let last_of_first = sends.iter().rposition(|(c, _)| c == &id).unwrap();
    assert_eq!(sends[last_of_first].1, None, "still sent as a reply");
    let order: Vec<String> = s
        .server
        .sends
        .lock()
        .unwrap()
        .iter()
        .map(|(_, _, b)| b.clone())
        .collect();
    assert_eq!(
        order.last().map(String::as_str),
        Some("second"),
        "{order:?}"
    );
}

/// A row that isn't failed keeps its quote (only a refused reply is changed).
#[tokio::test]
async fn retry_without_reply_leaves_a_pending_reply_alone() {
    let s = setup().await;
    s.session.send_replace(None);
    let id = s.outbox.enqueue("c1", "yes", reply(Q), None).await.unwrap();
    s.outbox.retry_without_reply(&id).await.unwrap();
    let p = s.outbox.pending("c1").await.unwrap();
    assert_eq!(p[0].reply_to_id.as_deref(), Some(Q));
}

/// Only a reply refused for its quote is changed: another failure (here an echo mismatch,
/// where the server did store the message) is left for Retry or Delete.
#[tokio::test]
async fn retry_without_reply_only_acts_on_a_gone_quote() {
    let s = setup().await;
    s.server.script([Answer::WrongEcho]);
    let id = s.outbox.enqueue("c1", "yes", reply(Q), None).await.unwrap();
    failed(&s).await;
    s.outbox.retry_without_reply(&id).await.unwrap();
    let p = s.outbox.pending("c1").await.unwrap();
    assert!(
        matches!(p[0].state, PendingState::Failed { .. }),
        "{:?}",
        p[0].state
    );
    assert_eq!(p[0].reply_to_id.as_deref(), Some(Q));
}

// ---- Queued files (attachments spec) ----

mod files {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{json, Value};
    use tokio::sync::{watch, Notify};

    use super::{eventually, open_db, NoSync, ME};
    use crate::cache::Cache;
    use crate::outbox::{
        FileRow, Outbox, OutboxError, Outgoing, PendingState, Post, SendFailure, Upload,
    };
    use crate::snapshot::SnapshotSource;
    use crate::store::Kind;
    use crate::transfer::{FileInfo, Flags, TransferId, Transfers, UploadSource};
    use crate::{InMemoryKeySlot, OutgoingFile};

    /// What the fake does with the next upload.
    enum Up {
        Ok,
        Fail(crate::Error),
        /// Wait for `gate`, stopping on the row's flags like a real transfer.
        Held,
        /// Wait for `gate`, then read the source; a read error comes back as a network
        /// failure, as reqwest reports a body stream that fails.
        HeldThenRead,
    }

    #[derive(Default)]
    struct Files {
        ups: Mutex<VecDeque<Up>>,
        /// Every upload: (file_client_id, the bytes read).
        uploaded: Mutex<Vec<(String, Vec<u8>)>>,
        posts: Mutex<Vec<Outgoing>>,
        post_fail: Mutex<VecDeque<SendFailure>>,
        gate: Notify,
        same_id: AtomicBool,
    }

    fn info(id: &str) -> FileInfo {
        FileInfo {
            id: id.into(),
            channel_id: "c1".into(),
            uploader_id: ME.into(),
            filename: "f".into(),
            original_name: "f".into(),
            size: 1,
            content_type: "x".into(),
            status: "committed".into(),
            sha256: None,
        }
    }

    fn api(code: &str) -> crate::Error {
        crate::Error::Api {
            code: code.into(),
            message: String::new(),
        }
    }

    #[async_trait::async_trait]
    impl Upload for Files {
        async fn upload(
            &self,
            _: TransferId,
            flags: &Arc<Flags>,
            _: &str,
            file: &FileRow,
            source: &SnapshotSource,
            _: u64,
        ) -> Result<FileInfo, crate::Error> {
            use tokio::io::AsyncReadExt;
            let next = self.ups.lock().unwrap().pop_front().unwrap_or(Up::Ok);
            if let Up::HeldThenRead = next {
                self.gate.notified().await;
                let mut bytes = vec![];
                let mut r = source.reader().await.map_err(|_| api("transfer.io"))?;
                return match r.read_to_end(&mut bytes).await {
                    Ok(_) => Ok(info(&format!("id-{}", file.file_client_id))),
                    Err(_) => Err(crate::Error::Timeout),
                };
            }
            let mut bytes = vec![];
            source
                .reader()
                .await
                .map_err(|_| api("transfer.io"))?
                .read_to_end(&mut bytes)
                .await
                // As reqwest reports a body stream that fails mid-send: a network error.
                .map_err(|_| crate::Error::Timeout)?;
            if let Up::Held = next {
                loop {
                    if flags.cancel.load(Ordering::SeqCst) {
                        return Err(api("transfer.cancelled"));
                    }
                    if flags.pause.load(Ordering::SeqCst) {
                        return Err(api("transfer.paused"));
                    }
                    if tokio::time::timeout(Duration::from_millis(20), self.gate.notified())
                        .await
                        .is_ok()
                    {
                        break;
                    }
                }
            }
            if let Up::Fail(e) = next {
                return Err(e);
            }
            self.uploaded
                .lock()
                .unwrap()
                .push((file.file_client_id.clone(), bytes));
            Ok(info(&if self.same_id.load(Ordering::SeqCst) {
                "dup".to_string()
            } else {
                format!("id-{}", file.file_client_id)
            }))
        }
    }

    #[async_trait::async_trait]
    impl Post for Files {
        async fn send(
            &self,
            ch: &str,
            msg: &Outgoing,
            cid: &str,
            _: u64,
        ) -> Result<Value, SendFailure> {
            self.posts.lock().unwrap().push(msg.clone());
            if let Some(f) = self.post_fail.lock().unwrap().pop_front() {
                return Err(f);
            }
            Ok(
                json!({ "id": format!("m-{cid}"), "channel_id": ch, "author_id": ME,
                       "body": msg.body, "created_at": "2026-09-26T10:00:00Z", "seq": 50,
                       "client_id": cid }),
            )
        }
    }

    struct S {
        outbox: Arc<Outbox>,
        files: Arc<Files>,
        transfers: Arc<Transfers>,
        cache: Arc<Cache>,
        session: watch::Sender<Option<u64>>,
        slot: Arc<InMemoryKeySlot>,
        dir: tempfile::TempDir,
        src: tempfile::TempDir,
        _cache_dir: tempfile::TempDir,
    }

    async fn open(
        s_dir: &std::path::Path,
        slot: &Arc<InMemoryKeySlot>,
        cache: &Arc<Cache>,
        files: &Arc<Files>,
        transfers: &Arc<Transfers>,
        rx: watch::Receiver<Option<u64>>,
    ) -> Arc<Outbox> {
        let outbox = Outbox::open(
            open_db(s_dir, Kind::Outbox, slot),
            cache.clone(),
            files.clone(),
            files.clone(),
            transfers.clone(),
            rx,
            s_dir,
        )
        .await
        .unwrap();
        outbox.set_chunk_for_tests(8);
        outbox
    }

    async fn setup() -> S {
        let slot = Arc::new(InMemoryKeySlot::default());
        let cache_dir = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(
            open_db(cache_dir.path(), Kind::Cache, &slot),
            ME.into(),
            Arc::new(NoSync),
            Arc::new(NoSync),
        );
        cache
            .live_event(
                "channel.update",
                &json!({ "id": "c1", "name": "c1", "seq": 1 }),
            )
            .await;
        let files = Arc::new(Files::default());
        let transfers = Arc::new(Transfers::new());
        let (session, rx) = watch::channel(Some(1));
        let outbox = open(dir.path(), &slot, &cache, &files, &transfers, rx).await;
        S {
            outbox,
            files,
            transfers,
            cache,
            session,
            slot,
            dir,
            src: tempfile::tempdir().unwrap(),
            _cache_dir: cache_dir,
        }
    }

    fn file(s: &S, name: &str, bytes: &[u8]) -> OutgoingFile {
        let path = s.src.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        OutgoingFile {
            path,
            filename: name.into(),
            content_type: "application/octet-stream".into(),
        }
    }

    fn snaps(s: &S) -> usize {
        std::fs::read_dir(s.outbox.snap_dir_for_tests())
            .unwrap()
            .count()
    }

    async fn drained(s: &S) {
        for _ in 0..500 {
            if s.outbox.unsent_count().await.unwrap() == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never drained: {:?}", s.outbox.pending("c1").await.unwrap());
    }

    async fn state(s: &S) -> PendingState {
        s.outbox.pending("c1").await.unwrap()[0].state.clone()
    }

    async fn until_state(s: &S, want: fn(&PendingState) -> bool) {
        for _ in 0..500 {
            if s.outbox
                .pending("c1")
                .await
                .unwrap()
                .first()
                .is_some_and(|m| want(&m.state))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "never reached the state: {:?}",
            s.outbox.pending("c1").await.unwrap()
        );
    }

    fn failed(p: &PendingState) -> bool {
        matches!(p, PendingState::Failed { .. })
    }

    /// Files go up in order, from the snapshots (a source edited after queueing changes
    /// nothing), then one POST names their ids in order with the body and reply target;
    /// after the ack no snapshot is left.
    #[tokio::test]
    async fn files_go_up_then_the_post_names_them_in_order() {
        let s = setup().await;
        s.session.send_replace(None);
        let (a, b) = (file(&s, "a", b"first file here"), file(&s, "b", b"second"));
        let r = s
            .outbox
            .enqueue_with_files("c1", "", Some("q".into()), None, vec![a.clone(), b])
            .await
            .unwrap();
        assert_eq!(r.files.len(), 2);
        assert_eq!(snaps(&s), 2);
        std::fs::write(&a.path, b"EDITED AFTERWARDS").unwrap();
        s.session.send_replace(Some(1));
        drained(&s).await;
        let up = s.files.uploaded.lock().unwrap().clone();
        assert_eq!(
            up.iter().map(|(_, b)| b.clone()).collect::<Vec<_>>(),
            vec![b"first file here".to_vec(), b"second".to_vec()]
        );
        let post = s.files.posts.lock().unwrap().last().cloned().unwrap();
        let want: Vec<String> = r
            .files
            .iter()
            .map(|f| format!("id-{}", f.file_client_id))
            .collect();
        assert_eq!(post.attachments, want);
        assert_eq!(post.reply_to_id.as_deref(), Some("q"));
        assert_eq!(post.body, "");
        for _ in 0..100 {
            if snaps(&s) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(snaps(&s), 0, "a snapshot survived the ack");
    }

    /// Refused before anything is copied: too many, too large, empty, or nothing at all.
    #[tokio::test]
    async fn limits_are_checked_before_anything_is_written() {
        let s = setup().await;
        s.session.send_replace(None);
        let eleven: Vec<_> = (0..11).map(|i| file(&s, &format!("f{i}"), b"x")).collect();
        assert_eq!(
            s.outbox
                .enqueue_with_files("c1", "", None, None, eleven)
                .await,
            Err(OutboxError::TooManyFiles)
        );
        let big = s.src.path().join("big");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(crate::MAX_FILE_BYTES + 1)
            .unwrap();
        let big = OutgoingFile {
            path: big,
            filename: "big".into(),
            content_type: "x".into(),
        };
        assert_eq!(
            s.outbox
                .enqueue_with_files("c1", "", None, None, vec![big])
                .await,
            Err(OutboxError::FileTooLarge)
        );
        assert_eq!(
            s.outbox
                .enqueue_with_files("c1", "", None, None, vec![file(&s, "e", b"")])
                .await,
            Err(OutboxError::EmptyFile)
        );
        assert_eq!(
            s.outbox
                .enqueue_with_files("c1", "  ", None, None, vec![])
                .await,
            Err(OutboxError::EmptyMessage)
        );
        assert_eq!(snaps(&s), 0);
        assert!(s.outbox.pending("c1").await.unwrap().is_empty());
        let ten: Vec<_> = (0..10).map(|i| file(&s, &format!("t{i}"), b"x")).collect();
        assert!(s
            .outbox
            .enqueue_with_files("c1", "", None, None, ten)
            .await
            .is_ok());
    }

    /// Two of three uploads done, then the process goes: the next one uploads the third only.
    #[tokio::test]
    async fn a_restart_resumes_after_the_last_upload() {
        let s = setup().await;
        s.files
            .ups
            .lock()
            .unwrap()
            .extend([Up::Ok, Up::Ok, Up::Held]);
        let fs: Vec<_> = (0..3).map(|i| file(&s, &format!("r{i}"), b"abc")).collect();
        let r = s
            .outbox
            .enqueue_with_files("c1", "x", None, None, fs)
            .await
            .unwrap();
        let files = s.files.clone();
        eventually("two uploads", move || {
            files.uploaded.lock().unwrap().len() == 2
        })
        .await;
        let outbox = s.outbox.clone();
        outbox.close().await;
        let (session, rx) = watch::channel(Some(2));
        let outbox = open(s.dir.path(), &s.slot, &s.cache, &s.files, &s.transfers, rx).await;
        outbox.resume().await.unwrap();
        for _ in 0..500 {
            if outbox.unsent_count().await.unwrap() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let up: Vec<String> = s
            .files
            .uploaded
            .lock()
            .unwrap()
            .iter()
            .map(|(f, _)| f.clone())
            .collect();
        assert_eq!(up.len(), 3, "{up:?}");
        assert_eq!(
            up[2], r.files[2].file_client_id,
            "the restart uploaded another file again"
        );
        drop(session);
    }

    /// Files swept meanwhile: the first `not_attachable` uploads them again, the second fails.
    #[tokio::test]
    async fn not_attachable_reuploads_once_then_fails() {
        let s = setup().await;
        s.files
            .post_fail
            .lock()
            .unwrap()
            .extend([SendFailure::Refused {
                code: "file.not_attachable".into(),
            }]);
        s.outbox
            .enqueue_with_files("c1", "x", None, None, vec![file(&s, "a", b"aa")])
            .await
            .unwrap();
        drained(&s).await;
        assert_eq!(
            s.files.uploaded.lock().unwrap().len(),
            2,
            "not uploaded again"
        );
        s.files.post_fail.lock().unwrap().extend([
            SendFailure::Refused {
                code: "file.not_attachable".into(),
            },
            SendFailure::Refused {
                code: "file.not_attachable".into(),
            },
        ]);
        s.outbox
            .enqueue_with_files("c1", "y", None, None, vec![file(&s, "b", b"bb")])
            .await
            .unwrap();
        until_state(&s, failed).await;
        assert_eq!(
            state(&s).await,
            PendingState::Failed {
                code: "file.not_attachable".into()
            }
        );
    }

    /// Two files the server gave one id: never POSTed, the row fails.
    #[tokio::test]
    async fn duplicate_ids_never_reach_the_post() {
        let s = setup().await;
        s.files.same_id.store(true, Ordering::SeqCst);
        s.outbox
            .enqueue_with_files(
                "c1",
                "x",
                None,
                None,
                vec![file(&s, "a", b"1"), file(&s, "b", b"2")],
            )
            .await
            .unwrap();
        until_state(&s, failed).await;
        assert_eq!(
            state(&s).await,
            PendingState::Failed {
                code: "outbox.duplicate_file".into()
            }
        );
        assert!(s.files.posts.lock().unwrap().is_empty());
    }

    /// Cancel through a file's transfer id fails the row; Retry sends it.
    #[tokio::test]
    async fn cancel_fails_the_row_and_retry_sends_it() {
        let s = setup().await;
        s.files.ups.lock().unwrap().push_back(Up::Held);
        let r = s
            .outbox
            .enqueue_with_files(
                "c1",
                "x",
                None,
                None,
                vec![file(&s, "a", b"1"), file(&s, "b", b"2")],
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Through the file that hasn't started: it still stops the row.
        s.transfers
            .flag(r.files[1].transfer_id)
            .cancel
            .store(true, Ordering::SeqCst);
        until_state(&s, failed).await;
        assert_eq!(
            state(&s).await,
            PendingState::Failed {
                code: "transfer.cancelled".into()
            }
        );
        assert!(s.files.posts.lock().unwrap().is_empty());
        s.outbox.retry(&r.client_id).await.unwrap();
        drained(&s).await;
        assert_eq!(s.files.posts.lock().unwrap().len(), 1);
    }

    /// Delete of an uploading row doesn't wait for the upload; its snapshots go.
    #[tokio::test]
    async fn delete_while_uploading_returns_at_once() {
        let s = setup().await;
        s.files.ups.lock().unwrap().push_back(Up::Held);
        let r = s
            .outbox
            .enqueue_with_files("c1", "x", None, None, vec![file(&s, "a", b"1")])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::time::timeout(
            Duration::from_secs(2),
            s.outbox.delete_pending(&r.client_id),
        )
        .await
        .expect("Delete waited for the upload")
        .unwrap();
        assert!(s.outbox.pending("c1").await.unwrap().is_empty());
        assert_eq!(snaps(&s), 0);
    }

    /// The same id again returns the stored receipt and copies nothing.
    #[tokio::test]
    async fn a_repeat_call_returns_the_stored_receipt() {
        let s = setup().await;
        s.session.send_replace(None);
        let first = s
            .outbox
            .enqueue_with_files("c1", "x", None, None, vec![file(&s, "a", b"1")])
            .await
            .unwrap();
        let mut events = s.transfers.events_for_tests();
        let again = s
            .outbox
            .enqueue_with_files(
                "c1",
                "x",
                None,
                Some(first.client_id.clone()),
                vec![file(&s, "a2", b"22"), file(&s, "b2", b"33")],
            )
            .await
            .unwrap();
        assert_eq!(again, first);
        assert_eq!(snaps(&s), 1);
        while let Ok(e) = events.try_recv() {
            assert_ne!(
                e.state,
                crate::transfer::TransferState::Preparing,
                "the repeat copied"
            );
        }
    }

    /// Snapshots no row names (a crash between the copy and the commit) go at the next open.
    #[tokio::test]
    async fn orphan_snapshots_go_at_open() {
        let s = setup().await;
        s.session.send_replace(None);
        let kept = s
            .outbox
            .enqueue_with_files("c1", "x", None, None, vec![file(&s, "a", b"1")])
            .await
            .unwrap();
        std::fs::write(
            s.outbox
                .snap_dir_for_tests()
                .join("0190a000-0000-7000-8000-00000000dead"),
            b"x",
        )
        .unwrap();
        let outbox = s.outbox.clone();
        outbox.close().await;
        let (_session, rx) = watch::channel(None);
        let outbox = open(s.dir.path(), &s.slot, &s.cache, &s.files, &s.transfers, rx).await;
        let names: Vec<String> = std::fs::read_dir(outbox.snap_dir_for_tests())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec![kept.files[0].file_client_id.clone()]);
    }

    /// A damaged snapshot fails the row before any byte is uploaded.
    #[tokio::test]
    async fn a_damaged_snapshot_fails_before_any_upload() {
        let s = setup().await;
        s.session.send_replace(None);
        let r = s
            .outbox
            .enqueue_with_files(
                "c1",
                "x",
                None,
                None,
                vec![file(&s, "a", b"some bytes here!")],
            )
            .await
            .unwrap();
        let path = s
            .outbox
            .snap_dir_for_tests()
            .join(&r.files[0].file_client_id);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[3] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        s.session.send_replace(Some(1));
        until_state(&s, failed).await;
        assert_eq!(
            state(&s).await,
            PendingState::Failed {
                code: "outbox.snapshot_damaged".into()
            }
        );
        assert!(s.files.uploaded.lock().unwrap().is_empty());
        let p = s.outbox.pending("c1").await.unwrap();
        assert_eq!(
            p[0].files[0].error.as_deref(),
            Some("outbox.snapshot_damaged")
        );
    }

    /// A 401 on an upload waits (the token is renewed), it doesn't fail the row.
    #[tokio::test]
    async fn a_401_on_an_upload_leaves_the_row_pending() {
        let s = setup().await;
        s.files
            .ups
            .lock()
            .unwrap()
            .push_back(Up::Fail(crate::Error::NotAuthenticated));
        s.outbox
            .enqueue_with_files("c1", "x", None, None, vec![file(&s, "a", b"1")])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!failed(&state(&s).await), "{:?}", state(&s).await);
        drained(&s).await;
    }

    /// Signing out pauses an upload: the row stays pending, and the next sign-in sends it.
    #[tokio::test]
    async fn sign_out_pauses_and_the_next_sign_in_resumes() {
        let s = setup().await;
        s.files.ups.lock().unwrap().push_back(Up::Held);
        s.outbox
            .enqueue_with_files("c1", "x", None, None, vec![file(&s, "a", b"1")])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        s.session.send_replace(None);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(state(&s).await, PendingState::Pending);
        s.session.send_replace(Some(2));
        drained(&s).await;
    }

    /// Per-file error on a permanent refusal, and it's cleared by Retry.
    #[tokio::test]
    async fn a_refused_file_is_named_and_retry_clears_it() {
        let s = setup().await;
        s.files
            .ups
            .lock()
            .unwrap()
            .push_back(Up::Fail(api("file.quota_exceeded")));
        let r = s
            .outbox
            .enqueue_with_files(
                "c1",
                "x",
                None,
                None,
                vec![file(&s, "a", b"1"), file(&s, "b", b"2")],
            )
            .await
            .unwrap();
        until_state(&s, failed).await;
        let p = s.outbox.pending("c1").await.unwrap();
        assert_eq!(p[0].files[0].error.as_deref(), Some("file.quota_exceeded"));
        assert_eq!(p[0].files[1].error, None);
        s.session.send_replace(None); // hold the resend, to see the row as Retry left it
        s.outbox.retry(&r.client_id).await.unwrap();
        let p = s.outbox.pending("c1").await.unwrap();
        assert_eq!(p[0].files[0].error, None, "Retry kept the old refusal");
        s.session.send_replace(Some(2));
        drained(&s).await;
    }

    /// Damage found mid-upload (after a good verification) fails the row as damaged at the
    /// next attempt, instead of retrying a network error forever.
    #[tokio::test]
    async fn damage_found_mid_upload_fails_the_row() {
        let s = setup().await;
        s.files.ups.lock().unwrap().push_back(Up::HeldThenRead);
        let r = s
            .outbox
            .enqueue_with_files(
                "c1",
                "x",
                None,
                None,
                vec![file(&s, "a", b"some bytes to damage")],
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await; // verified, now held
        let path = s
            .outbox
            .snap_dir_for_tests()
            .join(&r.files[0].file_client_id);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[5] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        s.files.gate.notify_one();
        until_state(&s, failed).await;
        assert_eq!(
            state(&s).await,
            PendingState::Failed {
                code: "outbox.snapshot_damaged".into()
            }
        );
    }

    /// The server has an accepted message: a cancel can't make it fail (or read as removed).
    #[tokio::test]
    async fn an_accepted_row_with_files_ignores_a_cancel() {
        let s = setup().await;
        s.session.send_replace(None);
        let r = s
            .outbox
            .enqueue_with_files("c1", "x", None, None, vec![file(&s, "a", b"1")])
            .await
            .unwrap();
        let cid = r.client_id.clone();
        s.outbox
            .db_for_tests()
            .call(move |c| {
                c.execute(
                    "UPDATE outbox SET state = 'accepted' WHERE client_id = ?1",
                    [&cid],
                )?;
                c.execute("UPDATE outbox_files SET file_id = 'fx'", [])
            })
            .await
            .unwrap();
        s.transfers
            .flag(r.files[0].transfer_id)
            .cancel
            .store(true, Ordering::SeqCst);
        s.session.send_replace(Some(1));
        // (An accepted row isn't "unsent": wait for it to leave.)
        for _ in 0..500 {
            if s.outbox.pending("c1").await.unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            s.outbox.pending("c1").await.unwrap().is_empty(),
            "the accepted row stayed"
        );
        assert!(
            s.files.uploaded.lock().unwrap().is_empty(),
            "an accepted row uploaded"
        );
        assert_eq!(
            s.files.posts.lock().unwrap()[0].attachments,
            vec!["fx".to_string()]
        );
    }

    /// A cancel while its files are being copied stops the copy: nothing is queued.
    #[tokio::test]
    async fn cancel_while_copying_queues_nothing() {
        let s = setup().await;
        s.session.send_replace(None);
        let mut events = s.transfers.events_for_tests();
        let transfers = s.transfers.clone();
        let watcher = tokio::spawn(async move {
            while let Ok(e) = events.recv().await {
                if e.state == crate::transfer::TransferState::Preparing {
                    transfers.flag(e.id).cancel.store(true, Ordering::SeqCst);
                    return;
                }
            }
        });
        let big = file(&s, "big", &vec![7u8; 8 * 64]); // 64 chunks of 8
        let got = s
            .outbox
            .enqueue_with_files("c1", "x", None, None, vec![big])
            .await;
        watcher.await.unwrap();
        assert_eq!(got, Err(OutboxError::Cancelled));
        assert!(s.outbox.pending("c1").await.unwrap().is_empty());
        assert_eq!(snaps(&s), 0);
    }

    /// A snapshot is read with the chunk size it was written with, whatever the current one.
    #[tokio::test]
    async fn a_snapshot_keeps_its_own_chunk_size() {
        let s = setup().await;
        s.session.send_replace(None);
        s.outbox
            .enqueue_with_files(
                "c1",
                "x",
                None,
                None,
                vec![file(&s, "a", b"twenty-one bytes long")],
            )
            .await
            .unwrap();
        s.outbox.set_chunk_for_tests(16);
        s.session.send_replace(Some(1));
        drained(&s).await;
        assert_eq!(
            s.files.uploaded.lock().unwrap()[0].1,
            b"twenty-one bytes long"
        );
    }
}
