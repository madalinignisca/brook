//! The sync loop (plan C2): a page and its cursor commit together, a bad page changes
//! nothing, `410` asks for a rebuild, and live events go through the same guard.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::store::{self, Db, Kind, Opened};
use crate::sync::{self, event_batch, Fetch, Page, SyncError, Synced};
use crate::{InMemoryKeySlot, KeySlot, KeyStore};

const ME: &str = "me";

struct Pages {
    pages: Mutex<Vec<Option<Value>>>, // None = 410
    asked: Mutex<Vec<String>>,
}

impl Pages {
    fn new(pages: Vec<Option<Value>>) -> Self {
        Self {
            pages: Mutex::new(pages),
            asked: Mutex::default(),
        }
    }
}

#[async_trait::async_trait]
impl Fetch for Pages {
    async fn page(&self, since: &str) -> Result<Page, crate::Error> {
        self.asked.lock().unwrap().push(since.to_string());
        match self.pages.lock().unwrap().remove(0) {
            Some(v) => Ok(Page::Rows(v)),
            None => Ok(Page::Reset),
        }
    }
}

fn open() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let slot: Arc<dyn KeySlot> = Arc::new(InMemoryKeySlot::default());
    match store::open(dir.path(), Kind::Cache, "s", &KeyStore::new(slot)).unwrap() {
        Opened::Ready { db, .. } => (db, dir),
        other => panic!("{other:?}"),
    }
}

fn page(next: &str, more: bool, messages: Vec<Value>) -> Option<Value> {
    Some(json!({
        "channels": [{ "id": "c", "name": "general", "seq": 5 }],
        "removed_channels": [],
        "memberships": [{ "channel_id": "c", "user_id": ME, "role": "member", "seq": 5 }],
        "left_members": [],
        "users": [{ "id": ME, "handle": "me", "display_name": "Me", "status": "active", "seq": 5 }],
        "messages": messages,
        "next": next,
        "more": more,
    }))
}

fn msg(id: &str, seq: i64, body: &str) -> Value {
    json!({ "id": id, "channel_id": "c", "author_id": "bob", "author_handle": "bob",
            "body": body, "created_at": "2026-09-25T10:00:00Z", "seq": seq })
}

async fn cursor(db: &Db) -> String {
    db.call(|c| c.query_row("SELECT cursor FROM meta WHERE id = 1", [], |r| r.get(0)))
        .await
        .unwrap()
}

async fn count(db: &Db, table: &'static str) -> i64 {
    db.call(move |c| c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)))
        .await
        .unwrap()
}

#[tokio::test]
async fn pages_apply_until_there_are_no_more() {
    let (db, _dir) = open();
    let fetch = Pages::new(vec![
        page("7", true, vec![msg("m1", 6, "one")]),
        page("9", false, vec![msg("m2", 8, "two")]),
    ]);
    let r = sync::run(&db, ME, &fetch).await.unwrap();
    assert!(
        matches!(r, Synced::Done(ref a) if a.channels.contains("c")),
        "{r:?}"
    );
    assert_eq!(*fetch.asked.lock().unwrap(), vec!["0", "7"]);
    assert_eq!(cursor(&db).await, "9");
    assert_eq!(count(&db, "messages").await, 2);
}

/// A page whose apply fails leaves nothing of itself, and the cursor where it was.
#[tokio::test]
async fn a_page_and_its_cursor_commit_together() {
    let (db, _dir) = open();
    db.call(|c| {
        c.execute_batch(
            "CREATE TEMP TRIGGER boom BEFORE INSERT ON messages WHEN NEW.id = 'boom'
             BEGIN SELECT RAISE(ABORT, 'boom'); END;",
        )
    })
    .await
    .unwrap();
    let fetch = Pages::new(vec![page(
        "7",
        false,
        vec![msg("m1", 6, "one"), msg("boom", 6, "x")],
    )]);
    assert!(matches!(
        sync::run(&db, ME, &fetch).await,
        Err(SyncError::Store(_))
    ));
    assert_eq!(cursor(&db).await, "0", "the cursor moved without its rows");
    assert_eq!(count(&db, "channels").await, 0, "half a page was applied");
    assert_eq!(count(&db, "messages").await, 0);
}

#[tokio::test]
async fn a_malformed_page_changes_nothing() {
    let (db, _dir) = open();
    for bad in [
        page("7", false, vec![json!({ "id": "m1", "seq": 6 })]), // no channel_id
        page("²", false, vec![]),                                // not an ASCII cursor
        Some(json!({ "channels": [] })),                         // missing lists
    ] {
        let fetch = Pages::new(vec![bad]);
        assert!(matches!(
            sync::run(&db, ME, &fetch).await,
            Err(SyncError::Malformed)
        ));
        assert_eq!(cursor(&db).await, "0");
        assert_eq!(count(&db, "channels").await, 0);
    }
}

#[tokio::test]
async fn a_reset_is_reported_and_moves_nothing() {
    let (db, _dir) = open();
    let fetch = Pages::new(vec![page("7", false, vec![]), None]);
    sync::run(&db, ME, &fetch).await.unwrap();
    assert_eq!(sync::run(&db, ME, &fetch).await.unwrap(), Synced::Reset);
    assert_eq!(cursor(&db).await, "7");
}

/// A live delete carries only ids: it turns the stored row into a tombstone that keeps its
/// author, and the server's own tombstone (same seq) arriving later changes nothing.
#[tokio::test]
async fn a_live_delete_patches_the_row_into_a_tombstone() {
    let (db, _dir) = open();
    let fetch = Pages::new(vec![page("7", false, vec![msg("m1", 6, "secret words")])]);
    sync::run(&db, ME, &fetch).await.unwrap();
    let batch = event_batch(
        "message.delete",
        &json!({ "id": "m1", "channel_id": "c", "seq": 9 }),
    )
    .unwrap();
    db.call(move |c| {
        let tx = c.transaction()?;
        crate::apply::apply(&tx, ME, &batch)?;
        tx.commit()
    })
    .await
    .unwrap();
    let (body, author, seq): (String, String, i64) = db
        .call(|c| {
            c.query_row(
                "SELECT json_extract(json, '$.body'), json_extract(json, '$.author_handle'), seq
                 FROM messages WHERE id = 'm1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })
        .await
        .unwrap();
    assert_eq!((body.as_str(), author.as_str(), seq), ("", "bob", 9));
}

#[test]
fn events_without_rows_ask_for_a_sync_instead() {
    let reaction = json!({ "message_id": "m1", "channel_id": "c", "emoji": "👍",
                           "user_id": "bob", "added": true, "count": 1, "seq": 9 });
    assert!(event_batch("reaction.update", &reaction).is_none());
    assert!(
        event_batch("message.new", &json!({ "id": "m1" })).is_none(),
        "no seq: not applied"
    );
}
