//! When a cached file goes (keep-offline spec §6): every cause drops its row and journals its
//! blob in the transaction that applies the cause.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::apply::{apply, Applied, Batch, MemberRow, MessageRow, Row};
use crate::store::{self, Db, Kind, Opened};
use crate::{InMemoryKeySlot, KeySlot, KeyStore};

const ME: &str = "me";

struct Cache {
    db: Db,
    _dir: tempfile::TempDir,
}

fn cache() -> Cache {
    let dir = tempfile::tempdir().unwrap();
    let slot: Arc<dyn KeySlot> = Arc::new(InMemoryKeySlot::default());
    match store::open(dir.path(), Kind::Cache, "s", &KeyStore::new(slot)).unwrap() {
        Opened::Ready { db, .. } => Cache { db, _dir: dir },
        other => panic!("{other:?}"),
    }
}

impl Cache {
    async fn apply(&self, batch: Batch) -> Applied {
        self.db
            .call(move |c| {
                let tx = c.transaction()?;
                let applied = apply(&tx, ME, &batch)?;
                tx.commit()?;
                Ok(applied)
            })
            .await
            .unwrap()
    }

    /// A blob of `file_id` is in the cache (as a download would leave it).
    async fn cached(&self, file_id: &'static str) {
        self.db
            .call(move |c| {
                c.execute(
                    "INSERT INTO files(file_id, sha256, size, key, chunk, state)
                     VALUES (?1, 'x', 1, x'00', 1048576, 'complete')",
                    [file_id],
                )
            })
            .await
            .unwrap();
    }

    async fn strings(&self, sql: &'static str) -> Vec<String> {
        self.db
            .call(move |c| {
                c.prepare(sql)?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()
            })
            .await
            .unwrap()
    }

    async fn files(&self) -> Vec<String> {
        self.strings("SELECT file_id FROM files ORDER BY file_id")
            .await
    }

    async fn journal(&self) -> Vec<String> {
        self.strings("SELECT path FROM deletions ORDER BY path")
            .await
    }

    async fn indexed(&self) -> Vec<String> {
        self.strings("SELECT file_id FROM message_files ORDER BY file_id")
            .await
    }
}

fn with_files(id: &str, channel: &str, seq: i64, files: &[&str]) -> MessageRow {
    let attachments: Vec<Value> = files.iter().map(|f| json!({ "id": f })).collect();
    MessageRow {
        id: id.into(),
        channel_id: channel.into(),
        seq,
        created_at: "2026-09-26T00:00:00Z".into(),
        json: json!({ "id": id, "channel_id": channel, "body": "", "seq": seq,
                      "attachments": attachments }),
    }
}

/// The caller in channel `c`, with message `m1` carrying files `f1` and `f2`, both cached.
async fn joined() -> Cache {
    let cache = cache();
    cache
        .apply(Batch {
            channels: vec![Row {
                id: "c".into(),
                seq: 10,
                json: json!({ "id": "c", "seq": 10 }),
            }],
            memberships: vec![MemberRow {
                channel_id: "c".into(),
                user_id: ME.into(),
                seq: 10,
                json: json!({ "channel_id": "c", "user_id": ME, "seq": 10 }),
            }],
            messages: vec![with_files("m1", "c", 10, &["f1", "f2"])],
            ..Batch::default()
        })
        .await;
    cache.cached("f1").await;
    cache.cached("f2").await;
    cache
}

#[tokio::test]
async fn a_file_deleted_from_its_message_leaves_the_cache() {
    let cache = joined().await;
    assert_eq!(cache.indexed().await, ["f1", "f2"]);
    // The server restamps the message with the shorter list (`DELETE /files/f2`).
    let applied = cache
        .apply(Batch {
            messages: vec![with_files("m1", "c", 20, &["f1"])],
            ..Batch::default()
        })
        .await;
    assert_eq!(applied.dropped_files, ["f2"]);
    assert_eq!(cache.files().await, ["f1"]);
    assert_eq!(cache.indexed().await, ["f1"]);
    assert_eq!(cache.journal().await, ["files/f2"]);
}

#[tokio::test]
async fn an_older_version_cant_bring_a_deleted_file_back() {
    let cache = joined().await;
    cache
        .apply(Batch {
            messages: vec![with_files("m1", "c", 20, &["f1"])],
            ..Batch::default()
        })
        .await;
    // A history page fetched before the delete, landing after it.
    let applied = cache
        .apply(Batch {
            messages: vec![with_files("m1", "c", 10, &["f1", "f2"])],
            history: true,
            ..Batch::default()
        })
        .await;
    assert!(applied.dropped_files.is_empty());
    assert_eq!(cache.indexed().await, ["f1"]);
}

#[tokio::test]
async fn a_live_delete_drops_every_file_of_the_message() {
    let cache = joined().await;
    let applied = cache
        .apply(Batch {
            tombstones: vec![("m1".into(), "c".into(), 30)],
            ..Batch::default()
        })
        .await;
    assert_eq!(applied.dropped_files, ["f1", "f2"]);
    assert!(cache.files().await.is_empty());
    assert!(cache.indexed().await.is_empty());
    assert_eq!(cache.journal().await, ["files/f1", "files/f2"]);
}

#[tokio::test]
async fn a_synced_tombstone_drops_them_too() {
    let cache = joined().await;
    let mut tombstone = with_files("m1", "c", 30, &[]);
    tombstone.json["deleted_at"] = json!("2026-09-26T01:00:00Z");
    cache
        .apply(Batch {
            messages: vec![tombstone],
            ..Batch::default()
        })
        .await;
    assert!(cache.files().await.is_empty());
    assert_eq!(cache.journal().await, ["files/f1", "files/f2"]);
}

#[tokio::test]
async fn a_removal_from_the_channel_drops_its_files() {
    let cache = joined().await;
    let applied = cache
        .apply(Batch {
            removed: vec![("c".into(), 20)],
            ..Batch::default()
        })
        .await;
    assert_eq!(applied.dropped_files, ["f1", "f2"]);
    assert!(cache.files().await.is_empty());
    assert!(cache.indexed().await.is_empty());
}

#[tokio::test]
async fn a_reset_drops_every_file_pinned_or_not() {
    let cache = joined().await;
    cache.cached("orphan").await; // cached, no message lists it (never happens, but covered)
    cache
        .db
        .call(|c| {
            c.execute("UPDATE files SET pinned = 1 WHERE file_id = 'f1'", [])?;
            let tx = c.transaction()?;
            crate::file_rows::drop_all(&tx)?;
            tx.commit()
        })
        .await
        .unwrap();
    assert!(cache.files().await.is_empty());
    assert!(cache.indexed().await.is_empty());
    assert_eq!(
        cache.journal().await,
        ["files/f1", "files/f2", "files/orphan"]
    );
}

#[tokio::test]
async fn a_file_listed_but_never_cached_journals_nothing() {
    let cache = joined().await;
    cache
        .apply(Batch {
            messages: vec![with_files("m2", "c", 12, &["f3"])],
            ..Batch::default()
        })
        .await;
    cache
        .apply(Batch {
            tombstones: vec![("m2".into(), "c".into(), 40)],
            ..Batch::default()
        })
        .await;
    assert!(cache.journal().await.is_empty());
    assert_eq!(cache.files().await, ["f1", "f2"]);
}
