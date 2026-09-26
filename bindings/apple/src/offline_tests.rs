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
        files: vec![],
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

struct Transfers(std::sync::Mutex<mpsc::Sender<Option<FfiTransferEvent>>>);

impl TransferListener for Transfers {
    fn on_transfer(&self, event: FfiTransferEvent) {
        let _ = self.0.lock().unwrap().send(Some(event));
    }
    fn on_resync(&self) {
        let _ = self.0.lock().unwrap().send(None);
    }
}

fn transfers(
    rx: broadcast::Receiver<brook_core::TransferEvent>,
) -> (Arc<Subscription>, mpsc::Receiver<Option<FfiTransferEvent>>) {
    let (tx, got) = mpsc::channel();
    let sub = deliver_transfers(rx, Arc::new(Transfers(std::sync::Mutex::new(tx))));
    (sub, got)
}

#[test]
fn transfer_states_cross_with_progress() {
    let (tx, rx) = broadcast::channel(16);
    let (_sub, got) = transfers(rx);
    let states = [
        brook_core::TransferState::Preparing,
        brook_core::TransferState::Running,
        brook_core::TransferState::Retrying { after_secs: 600 },
        brook_core::TransferState::Done,
        brook_core::TransferState::Cancelled,
        brook_core::TransferState::Failed("file.too_large".into()),
    ];
    for s in states {
        tx.send(brook_core::TransferEvent {
            id: brook_core::TransferId(7),
            done: 3,
            total: 9,
            state: s,
        })
        .unwrap();
    }
    let want = [
        FfiTransferState::Preparing,
        FfiTransferState::Running,
        FfiTransferState::Retrying { after_secs: 600 },
        FfiTransferState::Done,
        FfiTransferState::Cancelled,
        FfiTransferState::Failed {
            code: "file.too_large".into(),
        },
    ];
    for w in want {
        let e = got.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        assert_eq!((e.transfer_id, e.done, e.total, e.state), (7, 3, 9, w));
    }
}

/// Missed progress becomes one resync (re-read `pending_messages`), never a silent gap.
#[test]
fn missed_progress_is_a_resync() {
    let (tx, rx) = broadcast::channel(2);
    for n in 0..5 {
        tx.send(brook_core::TransferEvent {
            id: brook_core::TransferId(n),
            done: 0,
            total: 1,
            state: brook_core::TransferState::Running,
        })
        .unwrap();
    }
    let (_sub, got) = transfers(rx);
    assert_eq!(got.recv_timeout(Duration::from_secs(2)).unwrap(), None);
}

#[test]
fn a_receipt_and_pending_files_keep_their_ids() {
    let r = FfiSendReceipt::from(brook_core::SendReceipt {
        client_id: "c".into(),
        files: vec![brook_core::QueuedFile {
            file_client_id: "f".into(),
            transfer_id: brook_core::TransferId(42),
            size: 5,
        }],
    });
    assert_eq!(
        r.files,
        vec![FfiQueuedFile {
            file_client_id: "f".into(),
            transfer_id: 42,
            size: 5
        }]
    );
    let p = FfiPendingMessage::from(PendingMessage {
        client_id: "c".into(),
        channel_id: "ch".into(),
        body: String::new(),
        reply_to_id: None,
        files: vec![brook_core::PendingFile {
            file_client_id: "f".into(),
            transfer_id: brook_core::TransferId(42),
            filename: "a.pdf".into(),
            size: 5,
            uploaded: true,
            error: Some("file.quota_exceeded".into()),
        }],
        state: PendingState::Pending,
    });
    assert_eq!(
        p.files,
        vec![FfiPendingFile {
            file_client_id: "f".into(),
            transfer_id: 42,
            filename: "a.pdf".into(),
            size: 5,
            uploaded: true,
            error: Some("file.quota_exceeded".into()),
        }]
    );
}

fn wire_message(extra: serde_json::Value) -> brook_core::Message {
    let mut v = json!({
        "id": "m1", "channel_id": "c", "author_id": "u1", "author_display_name": "Al",
        "body": "hi", "created_at": "2026-09-26T10:00:00Z",
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    serde_json::from_value(v).unwrap()
}

#[test]
fn a_message_keeps_its_edit_quote_and_files() {
    let m = FfiMessage::from(wire_message(json!({
        "edited_at": "2026-09-26T11:00:00Z",
        "reply_to_id": "q",
        "reply_to": { "id": "q", "author_display_name": "Bo", "body": "(deleted)",
                      "deleted": true, "attachments": 0 },
        "attachments": [{ "id": "f1", "channel_id": "c", "uploader_id": "u1",
            "filename": "a.pdf", "original_name": "Ä.pdf", "size": 5,
            "content_type": "application/pdf", "status": "committed", "sha256": "ab" }],
    })));
    assert_eq!(m.edited_at.as_deref(), Some("2026-09-26T11:00:00Z"));
    assert_eq!(m.reply_to_id.as_deref(), Some("q"));
    let q = m.reply_to.unwrap();
    assert!(q.deleted);
    assert_eq!(
        (q.id.as_str(), q.author_display_name.as_deref()),
        ("q", Some("Bo"))
    );
    assert_eq!(m.attachments.len(), 1);
    let f = &m.attachments[0];
    assert_eq!(
        (f.filename.as_str(), f.original_name.as_str()),
        ("a.pdf", "Ä.pdf")
    );
    assert_eq!((f.size, f.sha256.as_deref()), (5, Some("ab")));
}

#[test]
fn a_tombstone_carries_no_files() {
    let m = FfiMessage::from(wire_message(json!({
        "deleted": true,
        "attachments": [{ "id": "f1", "channel_id": "c", "uploader_id": "u1",
            "filename": "a", "original_name": "a", "size": 1, "content_type": "x",
            "status": "committed" }],
    })));
    assert!(m.deleted);
    assert!(m.attachments.is_empty());
}

#[test]
fn message_events_cross_and_others_are_skipped() {
    use crate::call::FfiServerEvent;
    use crate::client::map_event;
    use brook_core::ServerEvent;
    assert!(matches!(
        map_event(ServerEvent::MessageNew(wire_message(json!({})))),
        Some(FfiServerEvent::MessageNew { message }) if message.id == "m1"
    ));
    assert!(matches!(
        map_event(ServerEvent::MessageUpdate(wire_message(json!({"body": "edited"})))),
        Some(FfiServerEvent::MessageUpdate { message }) if message.body == "edited"
    ));
    assert_eq!(
        map_event(ServerEvent::MessageDelete {
            channel_id: "c".into(),
            message_id: "m1".into()
        }),
        Some(FfiServerEvent::MessageDelete {
            channel_id: "c".into(),
            message_id: "m1".into()
        })
    );
    assert_eq!(map_event(ServerEvent::Ready), Some(FfiServerEvent::Ready));
}
