//! The sync loop (plan C2, spec §4.2) against the server's `GET /sync` (#91).
//!
//! Pages go through [`apply`](crate::apply::apply) and the new cursor is written **in the same
//! transaction**, so a crash mid-page leaves the old cursor and the replay is idempotent. The
//! cursor never advances from anything but `/sync`.

use serde_json::Value;

use crate::apply::{apply, Applied, Batch, MemberRow, MessageRow, Row};
use crate::store::{Db, StoreError};

/// One `/sync` answer.
pub(crate) enum Page {
    Rows(Value),
    /// `410 sync.reset`: the cursor means nothing to this server any more; rebuild the cache.
    Reset,
}

/// Where pages come from (the client's HTTP, or a test's list).
#[async_trait::async_trait]
pub(crate) trait Fetch: Send + Sync {
    async fn page(&self, since: &str) -> Result<Page, crate::Error>;
}

#[derive(Debug)]
pub(crate) enum SyncError {
    Store(StoreError),
    Net(crate::Error),
    /// A page we can't read: nothing applied, the cursor unchanged.
    Malformed,
}

/// How a sync run ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Synced {
    /// Caught up; what changed, for change notices.
    Done(Applied),
    /// The server said `410 sync.reset`: the caller rebuilds the cache (C5) and syncs from 0.
    Reset,
}

/// Fetch and apply pages until `more` is false.
pub(crate) async fn run(db: &Db, me: &str, fetch: &dyn Fetch) -> Result<Synced, SyncError> {
    let mut changed = Applied::default();
    loop {
        let since: String = db
            .call(|c| c.query_row("SELECT cursor FROM meta WHERE id = 1", [], |r| r.get(0)))
            .await
            .map_err(SyncError::Store)?;
        let page = match fetch.page(&since).await.map_err(SyncError::Net)? {
            Page::Reset => return Ok(Synced::Reset),
            Page::Rows(v) => v,
        };
        let (batch, next, more) = parse_page(&page).ok_or(SyncError::Malformed)?;
        let me = me.to_string();
        let applied = db
            .call(move |c| {
                let tx = c.transaction()?;
                let applied = apply(&tx, &me, &batch)?;
                // With the rows, or not at all.
                tx.execute("UPDATE meta SET cursor = ?1 WHERE id = 1", [&next])?;
                if !more {
                    // Caught up: ranges reach the top of this snapshot. (`next` is ASCII
                    // digits, checked by `parse_page`.)
                    let cursor: i64 = next.parse().unwrap_or(0);
                    crate::coverage::settle_tops(&tx, cursor)?;
                }
                tx.commit()?;
                Ok(applied)
            })
            .await
            .map_err(SyncError::Store)?;
        changed.channels.extend(applied.channels);
        changed.removed.extend(applied.removed);
        changed.users.extend(applied.users);
        if !more {
            return Ok(Synced::Done(changed));
        }
    }
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k)?.as_str().map(str::to_string)
}

fn seq(v: &Value) -> Option<i64> {
    v.get("seq")?.as_i64()
}

pub(crate) fn channel_row(v: &Value) -> Option<Row> {
    Some(Row {
        id: s(v, "id")?,
        seq: seq(v)?,
        json: v.clone(),
    })
}

pub(crate) fn message_row(v: &Value) -> Option<MessageRow> {
    Some(MessageRow {
        id: s(v, "id")?,
        channel_id: s(v, "channel_id")?,
        seq: seq(v)?,
        created_at: s(v, "created_at")?,
        json: v.clone(),
    })
}

fn member_row(v: &Value) -> Option<MemberRow> {
    Some(MemberRow {
        channel_id: s(v, "channel_id")?,
        user_id: s(v, "user_id")?,
        seq: seq(v)?,
        json: v.clone(),
    })
}

/// A `/sync` page as a batch, with its `next` cursor and `more`. Any malformed row fails the
/// whole page: applying part of one would advance the cursor past what was skipped.
pub(crate) fn parse_page(page: &Value) -> Option<(Batch, String, bool)> {
    let list = |k: &str| page.get(k).and_then(Value::as_array);
    let all =
        |k: &str, f: fn(&Value) -> Option<Row>| list(k)?.iter().map(f).collect::<Option<Vec<_>>>();
    let batch = Batch {
        channels: all("channels", channel_row)?,
        users: all("users", channel_row)?,
        memberships: list("memberships")?
            .iter()
            .map(member_row)
            .collect::<Option<_>>()?,
        messages: list("messages")?
            .iter()
            .map(message_row)
            .collect::<Option<_>>()?,
        tombstones: Vec::new(),
        history: false,
        history_floors: Default::default(),
        removed: list("removed_channels")?
            .iter()
            .map(|v| Some((s(v, "channel_id")?, seq(v)?)))
            .collect::<Option<_>>()?,
        left: list("left_members")?
            .iter()
            .map(|v| Some((s(v, "channel_id")?, s(v, "user_id")?, seq(v)?)))
            .collect::<Option<_>>()?,
    };
    let next = s(page, "next")?;
    // Cursors are ASCII digits (#91 answers 410 to anything else); never store another.
    if next.is_empty() || !next.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((batch, next, page.get("more")?.as_bool()?))
}

/// A live WebSocket event as a batch, if it carries rows (spec §4.1: live events go through
/// the same guard). Anything else (`reaction.update`, whose summary depends on who's asking;
/// events without a `seq`) applies nothing here: the caller runs a `/sync` instead.
pub(crate) fn event_batch(kind: &str, data: &Value) -> Option<Batch> {
    match kind {
        "message.new" | "message.update" => Some(Batch {
            messages: vec![message_row(data)?],
            ..Batch::default()
        }),
        "channel.update" => Some(Batch {
            channels: vec![channel_row(data)?],
            ..Batch::default()
        }),
        // A delete event carries only ids and seq: it patches the stored row.
        "message.delete" => Some(Batch {
            tombstones: vec![(s(data, "id")?, s(data, "channel_id")?, seq(data)?)],
            ..Batch::default()
        }),
        "channel.delete" => Some(Batch {
            removed: vec![(s(data, "id")?, seq(data)?)],
            ..Batch::default()
        }),
        _ => None,
    }
}
