//! Local data per user (plan C5): wipes erase keys and files, a user switch finds the other
//! users, a lost outbox is reported, and startup reconciliation erases only orphans.

use std::sync::Arc;

use crate::local::LocalData;
use crate::store::{Kind, Opened};
use crate::{InMemoryKeySlot, KeySlot, KeySlotError};

async fn open(root: &std::path::Path, slot: &Arc<InMemoryKeySlot>) -> LocalData {
    LocalData::open(root, slot.clone() as Arc<dyn KeySlot>)
        .await
        .unwrap()
        .expect("unlocked")
}

fn ready(o: Opened) -> crate::store::Db {
    match o {
        Opened::Ready { db, .. } => db,
        other => panic!("{other:?}"),
    }
}

fn dirs(root: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().unwrap().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    v.sort();
    v
}

#[tokio::test]
async fn a_wipe_erases_the_keys_and_the_files() {
    let root = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let local = open(root.path(), &slot).await;
    let stores = local.open_user("https://a", "u1").await.unwrap();
    let id = stores.store_id.clone();
    ready(stores.cache).close().await;
    ready(stores.outbox).close().await;
    assert!(slot.contains(&format!("cache:{id}")) && slot.contains(&format!("outbox:{id}")));
    local.wipe("https://a", "u1").await.unwrap();
    assert!(
        !slot.contains(&format!("cache:{id}")),
        "the key survived the wipe"
    );
    assert!(!slot.contains(&format!("outbox:{id}")));
    assert!(
        !root.path().join(&id).exists(),
        "the files survived the wipe"
    );
    // A later sign-in gets fresh stores, not the old ones.
    let again = local.open_user("https://a", "u1").await.unwrap();
    assert_ne!(again.store_id, id);
}

#[tokio::test]
async fn an_open_store_cant_be_wiped_under_its_user() {
    let root = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let local = open(root.path(), &slot).await;
    let stores = local.open_user("https://a", "u1").await.unwrap();
    // The cache closed, the outbox still open: the outbox's files stay under it.
    ready(stores.cache).close().await;
    assert!(
        local.wipe("https://a", "u1").await.is_err(),
        "wiped while open"
    );
    assert!(
        root.path()
            .join(&stores.store_id)
            .join("outbox.db")
            .exists(),
        "an open store's files were deleted"
    );
    ready(stores.outbox).close().await;
    local.wipe("https://a", "u1").await.unwrap();
}

/// The key store refuses to delete a key: the files still go, the wipe says it's
/// incomplete, and the row stays doomed. The next sign-in of that user finishes the erase
/// first and gets fresh stores (the old keys are never reused).
#[tokio::test]
async fn a_key_that_wont_go_keeps_the_wipe_pending() {
    let root = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let local = open(root.path(), &slot).await;
    let stores = local.open_user("https://a", "u1").await.unwrap();
    let id = stores.store_id.clone();
    ready(stores.cache).close().await;
    ready(stores.outbox).close().await;
    slot.fail_next("delete", KeySlotError::Unavailable);
    assert!(
        local.wipe("https://a", "u1").await.is_err(),
        "reported complete"
    );
    assert!(!root.path().join(&id).exists(), "the files stayed");
    assert!(
        slot.contains(&format!("cache:{id}")),
        "sanity: that key couldn't go"
    );
    let again = local.open_user("https://a", "u1").await.unwrap();
    assert_ne!(
        again.store_id, id,
        "the doomed store's id (and keys) came back"
    );
    assert!(
        !slot.contains(&format!("cache:{id}")),
        "the old key outlived the retry"
    );
}

#[tokio::test]
async fn a_user_switch_finds_the_other_users() {
    let root = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let local = open(root.path(), &slot).await;
    for (o, u) in [
        ("https://a", "u1"),
        ("https://a", "u2"),
        ("https://b", "u1"),
    ] {
        let s = local.open_user(o, u).await.unwrap();
        ready(s.cache).close().await;
        ready(s.outbox).close().await;
    }
    let mut others = local.others("https://a", "u1").await.unwrap();
    others.sort();
    assert_eq!(
        others,
        vec![
            ("https://a".to_string(), "u2".to_string()),
            ("https://b".to_string(), "u1".to_string())
        ]
    );
}

/// The outbox's key is gone: its unsent messages are lost, and the app is told so.
#[tokio::test]
async fn a_lost_outbox_is_reported() {
    let root = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let local = open(root.path(), &slot).await;
    let s = local.open_user("https://a", "u1").await.unwrap();
    let id = s.store_id.clone();
    assert!(!local.take_lost_unsent());
    ready(s.cache).close().await;
    ready(s.outbox).close().await;
    slot.put(&format!("outbox:{id}"), vec![9; 32]); // not the outbox's key
    let _s = local.open_user("https://a", "u1").await.unwrap();
    assert!(local.take_lost_unsent());
}

/// Startup: a directory no index row names (a wipe cut short) is erased; the rest, and
/// anything that isn't a store directory, is left alone.
#[tokio::test]
async fn reconciliation_erases_only_orphans() {
    let root = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let local = open(root.path(), &slot).await;
    let kept = local.open_user("https://a", "u1").await.unwrap();
    ready(kept.cache).close().await;
    ready(kept.outbox).close().await;
    let orphan = "ab".repeat(16);
    std::fs::create_dir(root.path().join(&orphan)).unwrap();
    std::fs::write(root.path().join(&orphan).join("cache.db"), b"old").unwrap();
    std::fs::create_dir(root.path().join("not-a-store")).unwrap();
    local.reconcile().await.unwrap();
    let mut want = vec![kept.store_id.clone(), "not-a-store".to_string()];
    want.sort();
    assert_eq!(dirs(root.path()), want);
}

/// Orphans `n` stores after the index key goes, the first holding `unsent` queued messages;
/// returns whether the reopened data said unsent messages were lost.
async fn lose_the_index_key(unsent: usize) -> bool {
    let root = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let local = open(root.path(), &slot).await;
    for (i, user) in ["u1", "u2"].into_iter().enumerate() {
        let s = local.open_user("https://a", user).await.unwrap();
        ready(s.cache).close().await;
        let outbox = ready(s.outbox);
        let rows = if i == 0 { unsent } else { 0 };
        outbox
            .call(move |c| {
                for n in 0..rows {
                    c.execute(
                        "INSERT INTO outbox(client_id, channel_id, body, state, created_at)
                         VALUES (?1, 'c', 'hi', 'queued', 'now')",
                        [format!("cid-{n}")],
                    )?;
                }
                // An accepted one is not unsent.
                c.execute(
                    "INSERT INTO outbox(client_id, channel_id, body, state, created_at)
                     VALUES ('done', 'c', 'hi', 'accepted', 'now')",
                    [],
                )
            })
            .await
            .unwrap();
        outbox.close().await;
    }
    local.close().await;
    slot.put("index", vec![5; 32]);
    let local = open(root.path(), &slot).await;
    assert!(dirs(root.path()).is_empty(), "{:?}", dirs(root.path()));
    let lost = local.take_lost_unsent();
    assert!(!local.take_lost_unsent(), "said twice");
    lost
}

/// The index's key is gone: nobody knows whose stores these are, so they're all erased, and
/// unsent messages among them are reported lost.
#[tokio::test]
async fn a_lost_index_key_orphans_every_store() {
    assert!(
        lose_the_index_key(2).await,
        "erased unsent messages went unmentioned"
    );
}

/// Only an outbox that held something is a loss.
#[tokio::test]
async fn orphaned_empty_outboxes_are_not_a_loss() {
    assert!(!lose_the_index_key(0).await, "a loss reported for nothing");
}

#[tokio::test]
async fn a_locked_index_means_no_local_data_and_nothing_deleted() {
    let root = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let local = open(root.path(), &slot).await;
    let s = local.open_user("https://a", "u1").await.unwrap();
    ready(s.cache).close().await;
    ready(s.outbox).close().await;
    local.close().await;
    slot.fail_next("load", KeySlotError::Unavailable);
    assert!(
        LocalData::open(root.path(), slot.clone() as Arc<dyn KeySlot>)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(dirs(root.path()).len(), 1);
    let _ = Kind::Cache;
}

/// An outbox in another format (pre-1.0: no migrations) is remade, and reported as lost.
#[tokio::test]
async fn an_outbox_in_another_format_is_reported_lost() {
    // (rows as `state`s): only something not yet accepted is a loss.
    for (states, lost) in [
        (vec!["pending"], true),
        (vec!["failed", "accepted"], true),
        (vec!["accepted"], false),
        (vec![], false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let slot = Arc::new(InMemoryKeySlot::default());
        let local = open(root.path(), &slot).await;
        let s = local.open_user("https://a", "u1").await.unwrap();
        ready(s.cache).close().await;
        let outbox = ready(s.outbox);
        let rows: Vec<String> = states.iter().map(|s| s.to_string()).collect();
        outbox
            .call(move |c| {
                for (n, state) in rows.iter().enumerate() {
                    c.execute(
                        "INSERT INTO outbox(client_id, channel_id, body, state, created_at)
                         VALUES (?1, 'c', 'hi', ?2, 'now')",
                        [format!("cid-{n}"), state.clone()],
                    )?;
                }
                c.execute("UPDATE meta SET format = 1", []) // the format before replies
            })
            .await
            .unwrap();
        outbox.close().await;
        let s = local.open_user("https://a", "u1").await.unwrap();
        assert_eq!(local.take_lost_unsent(), lost, "{states:?}");
        assert!(matches!(s.outbox, Opened::Ready { .. }));
        ready(s.cache).close().await;
        ready(s.outbox).close().await;
    }
}

/// An old outbox whose rows can't be counted counts as a loss: saying so wrongly beats
/// losing messages silently.
#[tokio::test]
async fn an_old_outbox_that_cant_be_counted_is_a_loss() {
    let root = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let local = open(root.path(), &slot).await;
    let s = local.open_user("https://a", "u1").await.unwrap();
    ready(s.cache).close().await;
    let outbox = ready(s.outbox);
    outbox
        .call(|c| {
            c.execute_batch("ALTER TABLE outbox RENAME TO elsewhere; UPDATE meta SET format = 1;")
        })
        .await
        .unwrap();
    outbox.close().await;
    let _s = local.open_user("https://a", "u1").await.unwrap();
    assert!(local.take_lost_unsent());
}
