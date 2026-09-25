//! History coverage (plan C2, spec §4.3 and §9 "Coverage").

use std::sync::Arc;

use serde_json::json;

use crate::apply::{apply, Batch, MemberRow, MessageRow, Row};
use crate::coverage::{self, Range};
use crate::store::{self, Db, Kind, Opened};
use crate::{InMemoryKeySlot, KeySlot, KeyStore};

fn open() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let slot: Arc<dyn KeySlot> = Arc::new(InMemoryKeySlot::default());
    match store::open(dir.path(), Kind::Cache, "s", &KeyStore::new(slot)).unwrap() {
        Opened::Ready { db, .. } => (db, dir),
        other => panic!("{other:?}"),
    }
}

fn msg(id: &str, seq: i64) -> MessageRow {
    MessageRow {
        id: id.into(),
        channel_id: "c".into(),
        seq,
        created_at: String::new(),
        json: json!({ "id": id, "seq": seq }),
    }
}

/// Runs `f` in a transaction and commits.
async fn tx<T: Send + 'static>(
    db: &Db,
    f: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<T> + Send + 'static,
) -> T {
    db.call(move |c| {
        let t = c.transaction()?;
        let out = f(&t)?;
        t.commit()?;
        Ok(out)
    })
    .await
    .unwrap()
}

async fn joined(db: &Db) {
    tx(db, |t| {
        apply(
            t,
            "me",
            &Batch {
                channels: vec![Row {
                    id: "c".into(),
                    seq: 1,
                    json: json!({}),
                }],
                memberships: vec![MemberRow {
                    channel_id: "c".into(),
                    user_id: "me".into(),
                    seq: 1,
                    json: json!({}),
                }],
                ..Batch::default()
            },
        )
    })
    .await;
}

async fn range(db: &Db) -> Option<Range> {
    tx(db, |t| coverage::range(t, "c")).await
}

fn ids(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// A live message lands in a channel never opened: it's kept, but it isn't coverage, so
/// opening the channel still fetches the head page, and then there is no gap below it.
#[tokio::test]
async fn a_live_message_is_not_coverage_and_the_head_fetch_leaves_no_gap() {
    let (db, _d) = open();
    joined(&db).await;
    tx(&db, |t| {
        apply(
            t,
            "me",
            &Batch {
                messages: vec![msg("m9", 9)],
                ..Batch::default()
            },
        )
    })
    .await;
    assert_eq!(range(&db).await, None, "a live message counted as coverage");
    // Open: the head page (history rows) holds m5..m9.
    let page = ids(&["m5", "m6", "m7", "m8", "m9"]);
    tx(&db, move |t| {
        apply(
            t,
            "me",
            &Batch {
                messages: page.iter().map(|i| msg(i, 0)).collect(),
                ..Batch::default()
            },
        )?;
        coverage::record_head(t, "c", &page, 5)
    })
    .await;
    let r = range(&db).await.unwrap();
    assert_eq!(
        (r.oldest_id.as_deref(), r.newest_id.as_deref()),
        (Some("m5"), Some("m9"))
    );
    assert!(!r.complete_to_start, "a full page isn't the start");
}

#[tokio::test]
async fn a_short_page_sets_complete_to_start() {
    let (db, _d) = open();
    joined(&db).await;
    tx(&db, |t| {
        coverage::record_head(t, "c", &ids(&["m3", "m4"]), 50)
    })
    .await;
    assert!(range(&db).await.unwrap().complete_to_start);
}

#[tokio::test]
async fn only_a_contiguous_older_page_extends_the_range() {
    let (db, _d) = open();
    joined(&db).await;
    tx(&db, |t| {
        coverage::record_head(t, "c", &ids(&["m5", "m6"]), 2)
    })
    .await;
    // Paged from somewhere else: not contiguous with the range.
    tx(&db, |t| {
        coverage::record_older(t, "c", "m9", &ids(&["m1"]), 2)
    })
    .await;
    assert_eq!(range(&db).await.unwrap().oldest_id.as_deref(), Some("m5"));
    tx(&db, |t| {
        coverage::record_older(t, "c", "m5", &ids(&["m3", "m4"]), 2)
    })
    .await;
    assert_eq!(range(&db).await.unwrap().oldest_id.as_deref(), Some("m3"));
    tx(&db, |t| {
        coverage::record_older(t, "c", "m3", &ids(&["m2"]), 2)
    })
    .await;
    let r = range(&db).await.unwrap();
    assert_eq!(r.oldest_id.as_deref(), Some("m2"));
    assert!(r.complete_to_start);
}

/// Only a completed sync moves the top: pages deliver by latest seq, not creation order,
/// and a live message alone could follow a missed one.
#[tokio::test]
async fn only_a_completed_sync_extends_the_top() {
    let (db, _d) = open();
    joined(&db).await;
    tx(&db, |t| {
        coverage::record_head(t, "c", &ids(&["m5", "m6"]), 2)
    })
    .await;
    // m8 arrives (live, or on a page that ends before m7's latest version).
    tx(&db, |t| {
        apply(
            t,
            "me",
            &Batch {
                messages: vec![msg("m8", 30)],
                ..Batch::default()
            },
        )
    })
    .await;
    assert_eq!(
        range(&db).await.unwrap().newest_id.as_deref(),
        Some("m6"),
        "a gap was covered"
    );
    // The run completes (m7 came on the last page): now the top is settled.
    tx(&db, |t| {
        apply(
            t,
            "me",
            &Batch {
                messages: vec![msg("m7", 40)],
                ..Batch::default()
            },
        )?;
        coverage::settle_tops(t)
    })
    .await;
    assert_eq!(range(&db).await.unwrap().newest_id.as_deref(), Some("m8"));
}

/// A delayed head page, wholly older than the range held now, doesn't join the two.
#[tokio::test]
async fn a_stale_head_page_does_not_bridge_a_gap() {
    let (db, _d) = open();
    joined(&db).await;
    tx(&db, |t| {
        coverage::record_head(t, "c", &ids(&["m10", "m11"]), 2)
    })
    .await;
    tx(&db, |t| {
        coverage::record_head(t, "c", &ids(&["m01", "m02"]), 2)
    })
    .await;
    let r = range(&db).await.unwrap();
    assert_eq!(
        (r.oldest_id.as_deref(), r.newest_id.as_deref()),
        (Some("m10"), Some("m11"))
    );
}

#[tokio::test]
async fn an_empty_covered_channel_gets_its_first_message() {
    let (db, _d) = open();
    joined(&db).await;
    tx(&db, |t| coverage::record_head(t, "c", &[], 50)).await;
    tx(&db, |t| {
        apply(
            t,
            "me",
            &Batch {
                messages: vec![msg("m1", 3)],
                ..Batch::default()
            },
        )?;
        coverage::settle_tops(t)
    })
    .await;
    let r = range(&db).await.unwrap();
    assert_eq!(
        (r.oldest_id.as_deref(), r.newest_id.as_deref()),
        (Some("m1"), Some("m1"))
    );
    assert!(r.complete_to_start);
}
