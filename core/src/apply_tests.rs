//! Ordering rules of the one apply function (plan C2, spec §4.1 and §9 "Ordering").

use std::sync::Arc;

use serde_json::json;

use crate::apply::{apply, Batch, MemberRow, MessageRow, Row};
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
    async fn apply(&self, batch: Batch) {
        self.db
            .call(move |c| {
                let tx = c.transaction()?;
                apply(&tx, ME, &batch)?;
                tx.commit()
            })
            .await
            .unwrap();
    }

    async fn one(&self, sql: &'static str, arg: &'static str) -> Option<String> {
        self.db
            .call(move |c| {
                use rusqlite::OptionalExtension;
                c.query_row(sql, [arg], |r| r.get::<_, String>(0))
                    .optional()
            })
            .await
            .unwrap()
    }

    async fn channel_name(&self, id: &'static str) -> Option<String> {
        self.one(
            "SELECT json_extract(json, '$.name') FROM channels WHERE id = ?1",
            id,
        )
        .await
    }

    async fn body(&self, id: &'static str) -> Option<String> {
        self.one(
            "SELECT json_extract(json, '$.body') FROM messages WHERE id = ?1",
            id,
        )
        .await
    }

    async fn active_members(&self, channel: &'static str) -> Vec<String> {
        self.db
            .call(move |c| {
                c.prepare("SELECT user_id FROM memberships WHERE channel_id = ?1 AND left = 0 ORDER BY user_id")?
                    .query_map([channel], |r| r.get(0))?
                    .collect()
            })
            .await
            .unwrap()
    }
}

fn channel(id: &str, seq: i64, name: &str) -> Row {
    Row {
        id: id.into(),
        seq,
        json: json!({ "id": id, "name": name, "seq": seq }),
    }
}

fn member(channel: &str, user: &str, seq: i64) -> MemberRow {
    MemberRow {
        channel_id: channel.into(),
        user_id: user.into(),
        seq,
        json: json!({ "channel_id": channel, "user_id": user, "seq": seq }),
    }
}

fn message(id: &str, channel: &str, seq: i64, body: &str) -> MessageRow {
    MessageRow {
        id: id.into(),
        channel_id: channel.into(),
        seq,
        created_at: "2026-09-25T00:00:00Z".into(),
        json: json!({ "id": id, "channel_id": channel, "body": body, "seq": seq }),
    }
}

/// The caller in channel `c` at seq 10, with one message.
async fn joined() -> Cache {
    let cache = cache();
    cache
        .apply(Batch {
            channels: vec![channel("c", 10, "general")],
            memberships: vec![member("c", ME, 10), member("c", "bob", 10)],
            messages: vec![message("m1", "c", 10, "hello")],
            ..Batch::default()
        })
        .await;
    cache
}

#[tokio::test]
async fn a_stale_page_after_a_live_event_keeps_the_live_row() {
    let cache = joined().await;
    cache
        .apply(Batch {
            messages: vec![message("m1", "c", 30, "edited live")],
            ..Batch::default()
        })
        .await;
    cache
        .apply(Batch {
            messages: vec![message("m1", "c", 20, "older page")],
            ..Batch::default()
        })
        .await;
    assert_eq!(cache.body("m1").await.as_deref(), Some("edited live"));
}

#[tokio::test]
async fn an_ack_after_a_tombstone_keeps_the_tombstone() {
    let cache = joined().await;
    cache
        .apply(Batch {
            messages: vec![message("m1", "c", 40, "")],
            ..Batch::default()
        })
        .await;
    cache
        .apply(Batch {
            messages: vec![message("m1", "c", 10, "hello")],
            ..Batch::default()
        })
        .await;
    assert_eq!(cache.body("m1").await.as_deref(), Some(""));
}

#[tokio::test]
async fn a_removal_deletes_the_channel_and_a_late_event_cant_bring_it_back() {
    let cache = joined().await;
    cache
        .apply(Batch {
            removed: vec![("c".into(), 20)],
            ..Batch::default()
        })
        .await;
    assert_eq!(cache.channel_name("c").await, None);
    assert_eq!(cache.body("m1").await, None);
    // A late event and a late page, both from before the removal.
    cache
        .apply(Batch {
            messages: vec![message("m2", "c", 15, "late")],
            ..Batch::default()
        })
        .await;
    cache
        .apply(Batch {
            channels: vec![channel("c", 18, "general")],
            memberships: vec![member("c", "bob", 18)],
            ..Batch::default()
        })
        .await;
    assert_eq!(
        cache.channel_name("c").await,
        None,
        "a late page resurrected the channel"
    );
    assert_eq!(cache.body("m2").await, None);
}

#[tokio::test]
async fn a_history_page_after_a_removal_is_dropped() {
    let cache = joined().await;
    cache
        .apply(Batch {
            removed: vec![("c".into(), 20)],
            ..Batch::default()
        })
        .await;
    cache
        .apply(Batch {
            messages: vec![message("old", "c", 0, "history")],
            ..Batch::default()
        })
        .await;
    assert_eq!(cache.body("old").await, None);
}

#[tokio::test]
async fn a_history_row_never_replaces_a_stored_one() {
    let cache = joined().await;
    cache
        .apply(Batch {
            messages: vec![message("m1", "c", 0, "from history")],
            ..Batch::default()
        })
        .await;
    assert_eq!(cache.body("m1").await.as_deref(), Some("hello"));
    cache
        .apply(Batch {
            messages: vec![message("h", "c", 0, "older")],
            ..Batch::default()
        })
        .await;
    assert_eq!(
        cache.body("h").await.as_deref(),
        Some("older"),
        "a history row for a new message"
    );
}

/// A rejoin page: the caller's membership (above the fence) plus the channel's rows with
/// their original, lower seq (#91 sends those whatever their seq). All of it lands.
#[tokio::test]
async fn a_rejoin_page_lands_whole_even_with_lower_seq_rows() {
    let cache = joined().await;
    cache
        .apply(Batch {
            removed: vec![("c".into(), 20)],
            ..Batch::default()
        })
        .await;
    cache
        .apply(Batch {
            channels: vec![channel("c", 5, "general")],
            memberships: vec![member("c", "bob", 10), member("c", ME, 30)],
            messages: vec![message("m3", "c", 25, "after rejoin")],
            ..Batch::default()
        })
        .await;
    assert_eq!(cache.channel_name("c").await.as_deref(), Some("general"));
    assert_eq!(cache.active_members("c").await, vec!["bob", ME]);
    assert_eq!(cache.body("m3").await.as_deref(), Some("after rejoin"));
}

#[tokio::test]
async fn a_stale_removal_after_a_rejoin_keeps_the_channel() {
    let cache = joined().await;
    cache
        .apply(Batch {
            removed: vec![("c".into(), 20)],
            ..Batch::default()
        })
        .await;
    cache
        .apply(Batch {
            channels: vec![channel("c", 30, "general")],
            memberships: vec![member("c", ME, 30)],
            ..Batch::default()
        })
        .await;
    cache
        .apply(Batch {
            removed: vec![("c".into(), 20)],
            ..Batch::default()
        })
        .await;
    assert_eq!(cache.channel_name("c").await.as_deref(), Some("general"));
}

/// Only the caller's own membership says "I'm back": a newer channel or other-member row
/// doesn't stop the caller's removal.
#[tokio::test]
async fn only_my_membership_can_outrank_my_removal() {
    let cache = joined().await;
    cache
        .apply(Batch {
            channels: vec![channel("c", 40, "renamed")],
            memberships: vec![member("c", "bob", 40)],
            ..Batch::default()
        })
        .await;
    cache
        .apply(Batch {
            removed: vec![("c".into(), 30)],
            ..Batch::default()
        })
        .await;
    assert_eq!(
        cache.channel_name("c").await,
        None,
        "another row outranked my removal"
    );
}

#[tokio::test]
async fn a_departure_keeps_its_seq_against_an_older_membership_row() {
    let cache = joined().await;
    cache
        .apply(Batch {
            left: vec![("c".into(), "bob".into(), 20)],
            ..Batch::default()
        })
        .await;
    assert_eq!(cache.active_members("c").await, vec![ME]);
    cache
        .apply(Batch {
            memberships: vec![member("c", "bob", 10)],
            ..Batch::default()
        })
        .await;
    assert_eq!(
        cache.active_members("c").await,
        vec![ME],
        "an older row resurrected bob"
    );
    cache
        .apply(Batch {
            memberships: vec![member("c", "bob", 25)],
            ..Batch::default()
        })
        .await;
    assert_eq!(
        cache.active_members("c").await,
        vec!["bob", ME],
        "a real rejoin is honoured"
    );
}

/// A member row the cache still holds when the caller rejoins (a leftover the removal
/// didn't cover) but that isn't in the rejoin page's complete member list: marked left.
#[tokio::test]
async fn a_rejoin_reconciles_members_who_left_meanwhile() {
    let cache = joined().await;
    cache
        .apply(Batch {
            removed: vec![("c".into(), 20)],
            ..Batch::default()
        })
        .await;
    cache
        .db
        .call(|c| {
            c.execute(
                "INSERT INTO memberships(channel_id, user_id, seq, left, json) VALUES ('c', 'ghost', 12, 0, '{}')",
                [],
            )
        })
        .await
        .unwrap();
    cache
        .apply(Batch {
            channels: vec![channel("c", 40, "general")],
            memberships: vec![member("c", ME, 40), member("c", "carol", 12)],
            ..Batch::default()
        })
        .await;
    assert_eq!(cache.active_members("c").await, vec!["carol", ME]);
}

#[tokio::test]
async fn replaying_a_page_changes_nothing() {
    let cache = joined().await;
    let page = || Batch {
        channels: vec![channel("c", 30, "renamed")],
        memberships: vec![member("c", "dave", 30)],
        messages: vec![message("m1", "c", 30, "edit")],
        left: vec![("c".into(), "bob".into(), 30)],
        ..Batch::default()
    };
    cache.apply(page()).await;
    let before = (
        cache.channel_name("c").await,
        cache.body("m1").await,
        cache.active_members("c").await,
    );
    cache.apply(page()).await;
    let after = (
        cache.channel_name("c").await,
        cache.body("m1").await,
        cache.active_members("c").await,
    );
    assert_eq!(before, after);
}

#[tokio::test]
async fn a_stale_channel_or_user_row_never_replaces_a_newer_one() {
    let cache = joined().await;
    let user = |seq: i64, name: &str| Row {
        id: "bob".into(),
        seq,
        json: json!({ "id": "bob", "display_name": name, "seq": seq }),
    };
    cache
        .apply(Batch {
            channels: vec![channel("c", 30, "renamed")],
            users: vec![user(30, "Robert")],
            ..Batch::default()
        })
        .await;
    cache
        .apply(Batch {
            channels: vec![channel("c", 20, "general")],
            users: vec![user(20, "Bob")],
            ..Batch::default()
        })
        .await;
    assert_eq!(cache.channel_name("c").await.as_deref(), Some("renamed"));
    assert_eq!(
        cache
            .one(
                "SELECT json_extract(json, '$.display_name') FROM users WHERE id = ?1",
                "bob"
            )
            .await
            .as_deref(),
        Some("Robert")
    );
}
