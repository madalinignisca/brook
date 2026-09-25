//! The offline records and listeners across the FFI: what Swift draws must survive the
//! mapping, and a slow listener must be told to re-read rather than skip silently.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use brook_core::{CacheEvent, CacheState, Deleted, PendingMessage, PendingState};
use serde_json::json;
use tokio::sync::broadcast;

use super::*;

struct Collect(std::sync::Mutex<mpsc::Sender<FfiCacheEvent>>);

impl CacheEventListener for Collect {
    fn on_cache_event(&self, event: FfiCacheEvent) {
        let _ = self.0.lock().unwrap().send(event);
    }
}

fn listen(
    rx: broadcast::Receiver<CacheEvent>,
) -> (Arc<Subscription>, mpsc::Receiver<FfiCacheEvent>) {
    let (tx, got) = mpsc::channel();
    let sub = deliver_cache_events(rx, Arc::new(Collect(std::sync::Mutex::new(tx))));
    (sub, got)
}

#[test]
fn every_cache_event_crosses_with_its_ids() {
    let (tx, rx) = broadcast::channel(16);
    let (_sub, got) = listen(rx);
    let sent = [
        CacheEvent::Channels(vec!["c".into()]),
        CacheEvent::Removed(vec!["r".into()]),
        CacheEvent::Users(vec!["u".into()]),
        CacheEvent::Reset,
        CacheEvent::Outbox("o".into()),
        CacheEvent::OutboxLost,
    ];
    for e in sent {
        tx.send(e).unwrap();
    }
    let want = vec![
        FfiCacheEvent::Channels {
            ids: vec!["c".into()],
        },
        FfiCacheEvent::Removed {
            ids: vec!["r".into()],
        },
        FfiCacheEvent::Users {
            ids: vec!["u".into()],
        },
        FfiCacheEvent::Reset,
        FfiCacheEvent::Outbox {
            channel_id: "o".into(),
        },
        FfiCacheEvent::OutboxLost,
    ];
    let seen: Vec<_> = (0..want.len())
        .map(|_| {
            got.recv_timeout(Duration::from_secs(2))
                .expect("an event went missing")
        })
        .collect();
    assert_eq!(seen, want);
}

/// Notices missed by a slow listener arrive as one `Reset` (re-read everything).
#[test]
fn a_lagging_listener_is_told_to_reread() {
    let (tx, rx) = broadcast::channel(2);
    for n in 0..5 {
        tx.send(CacheEvent::Outbox(format!("c{n}"))).unwrap(); // overflows before delivery
    }
    let (_sub, got) = listen(rx);
    assert_eq!(
        got.recv_timeout(Duration::from_secs(2)).unwrap(),
        FfiCacheEvent::Reset,
        "missed notices went unmentioned"
    );
}

#[test]
fn a_cached_channel_keeps_its_unread_count_and_members() {
    let mut ch: brook_core::Channel = serde_json::from_value(json!({
        "id": "d", "kind": "dm", "name": null, "archived": false,
        "members": [{ "id": "u2", "handle": "bob", "display_name": "Bob" }],
    }))
    .unwrap();
    ch.unread_count = 3;
    let f = FfiCachedChannel::from(ch);
    assert_eq!(f.unread_count, 3);
    assert_eq!(
        f.members,
        vec![FfiMember {
            id: "u2".into(),
            handle: "bob".into(),
            display_name: "Bob".into()
        }]
    );
}

#[test]
fn a_message_keeps_its_client_id_and_a_tombstone_its_place() {
    let m: brook_core::Message = serde_json::from_value(json!({
        "id": "m1", "channel_id": "c", "author_id": "u1", "author_handle": "al",
        "author_display_name": "Al", "body": "hi", "created_at": "2026-09-25T10:00:00Z",
        "client_id": "cid-1",
    }))
    .unwrap();
    let f = FfiMessage::from(m);
    assert_eq!(f.client_id.as_deref(), Some("cid-1"));
    assert!(!f.deleted);
    assert_eq!(f.body, "hi");
    let gone: brook_core::Message = serde_json::from_value(json!({
        "id": "m2", "channel_id": "c", "body": "secret", "deleted": true,
    }))
    .unwrap();
    let f = FfiMessage::from(gone);
    assert!(f.deleted, "a tombstone read as a message");
    assert_eq!(f.body, "", "a deleted message's text crossed the FFI");
}

#[test]
fn every_pending_state_and_delete_outcome_maps() {
    let p = |state| PendingMessage {
        client_id: "x".into(),
        channel_id: "c".into(),
        body: "b".into(),
        reply_to_id: Some("q".into()),
        state,
    };
    assert_eq!(
        FfiPendingMessage::from(p(PendingState::Pending))
            .reply_to_id
            .as_deref(),
        Some("q"),
        "a queued reply lost its target"
    );
    let states: Vec<_> = [
        PendingState::Pending,
        PendingState::Sending,
        PendingState::Accepted,
        PendingState::Failed {
            code: "not_found".into(),
        },
    ]
    .into_iter()
    .map(|s| FfiPendingMessage::from(p(s)).state)
    .collect();
    assert_eq!(
        states,
        vec![
            FfiPendingState::Pending,
            FfiPendingState::Sending,
            FfiPendingState::Accepted,
            FfiPendingState::Failed {
                code: "not_found".into()
            },
        ]
    );
    assert_eq!(FfiDeleted::from(Deleted::Removed), FfiDeleted::Removed);
    assert_eq!(
        FfiDeleted::from(Deleted::AlreadySent),
        FfiDeleted::AlreadySent
    );
    assert_eq!(FfiDeleted::from(Deleted::NotFound), FfiDeleted::NotFound);
}

#[test]
fn the_cache_state_crosses_in_unix_milliseconds() {
    let s = CacheState {
        syncing: true,
        last_synced: Some(UNIX_EPOCH + Duration::from_millis(1_700_000_000_123)),
        offline: true,
    };
    assert_eq!(
        FfiCacheState::from(s),
        FfiCacheState {
            syncing: true,
            last_synced_unix_ms: Some(1_700_000_000_123),
            offline: true,
        }
    );
    assert_eq!(
        FfiCacheState::from(CacheState::default()),
        FfiCacheState::default()
    );
}
