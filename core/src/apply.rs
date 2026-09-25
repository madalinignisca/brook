//! The one way rows enter the cache (plan C2, spec §4.1). `/sync` pages, live events,
//! history pages and send acknowledgements all become a [`Batch`] and go through [`apply`]
//! inside one cache.db transaction.
//!
//! The rules, in the order `apply` runs them:
//! 1. **The caller's own membership rows first.** One with a higher `seq` than an active
//!    removal fence lifts it, so a rejoin page's other rows (sent with their original, lower
//!    `seq`, #91) land after the fence is gone, in the same transaction.
//! 2. **Removals** apply only above the caller's own membership `seq`. They fence the channel
//!    and delete its rows. Channel metadata or another member's newer row is not evidence
//!    that the caller is back. The fence stays after a rejoin as a **floor**: a message row
//!    from before the removal is never current, so it's never stored again.
//! 3. **Everything else** through the per-row guard: a row replaces the stored one only with
//!    a higher `seq`, and a fenced channel's rows apply only above the fence.
//! 4. **Messages:** history rows (`seq = 0`) land only in a present, unfenced channel and
//!    never over a stored row. A live delete patches the row (or leaves a partial tombstone),
//!    and every reply quoting a changed message follows it, so deleted words don't survive
//!    in an excerpt. The server's full row replaces a partial one at the same `seq`.
//!
//! Coverage (`coverage.rs`) is not moved here: `/sync` delivers rows by their latest `seq`,
//! not in creation order, so contiguity is only known once a sync run completes.

use std::collections::HashSet;

use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::Value;

/// A row as the server sent it, kept whole as JSON; `id` and `seq` index it.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    pub(crate) id: String,
    pub(crate) seq: i64,
    pub(crate) json: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct MemberRow {
    pub(crate) channel_id: String,
    pub(crate) user_id: String,
    pub(crate) seq: i64,
    pub(crate) json: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct MessageRow {
    pub(crate) id: String,
    pub(crate) channel_id: String,
    pub(crate) seq: i64,
    pub(crate) created_at: String,
    pub(crate) json: Value,
}

/// Rows from one source, applied together.
#[derive(Debug, Default)]
pub(crate) struct Batch {
    pub(crate) channels: Vec<Row>,
    pub(crate) memberships: Vec<MemberRow>,
    pub(crate) users: Vec<Row>,
    pub(crate) messages: Vec<MessageRow>,
    /// The caller's removals: `(channel id, seq)`.
    pub(crate) removed: Vec<(String, i64)>,
    /// Other members' departures: `(channel id, user id, seq)`.
    pub(crate) left: Vec<(String, String, i64)>,
    /// Live deletes, which carry only ids: `(message id, channel id, seq)`. They patch the
    /// stored row into a tombstone (the author and time stay), never replace it.
    pub(crate) tombstones: Vec<(String, String, i64)>,
}

/// What `apply` changed, for change notices (`CacheEvent`).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    pub(crate) channels: HashSet<String>,
    pub(crate) removed: HashSet<String>,
}

/// Apply `batch` for the signed-in user `me`. The caller commits (with the cursor, for a
/// `/sync` page).
pub(crate) fn apply(tx: &Transaction<'_>, me: &str, batch: &Batch) -> rusqlite::Result<Applied> {
    let mut applied = Applied::default();

    // 1. The caller's own memberships. Above an active fence, one clears it (the fence stays
    //    as a floor: see `floor_of`).
    for m in batch.memberships.iter().filter(|m| m.user_id == me) {
        let fence = fence_of(tx, &m.channel_id)?;
        if fence.is_some_and(|f| m.seq <= f) {
            continue; // older than the removal: stale
        }
        if upsert_membership(tx, m)? {
            applied.channels.insert(m.channel_id.clone());
            if fence.is_some() {
                tx.execute(
                    "UPDATE removed SET active = 0 WHERE channel_id = ?1",
                    [&m.channel_id],
                )?;
            }
        }
    }

    // 2. Removals, only above the caller's own membership.
    for (channel_id, seq) in &batch.removed {
        let mine: Option<i64> = tx
            .query_row(
                "SELECT seq FROM memberships WHERE channel_id = ?1 AND user_id = ?2 AND left = 0",
                params![channel_id, me],
                |r| r.get(0),
            )
            .optional()?;
        if mine.is_some_and(|m| *seq <= m) {
            continue; // the caller rejoined after this removal
        }
        if floor_of(tx, channel_id)?.is_some_and(|f| *seq <= f) {
            continue;
        }
        tx.execute(
            "INSERT INTO removed(channel_id, seq, active) VALUES (?1, ?2, 1)
             ON CONFLICT(channel_id) DO UPDATE SET seq = excluded.seq, active = 1",
            params![channel_id, seq],
        )?;
        remove_channel_rows(tx, channel_id)?;
        applied.removed.insert(channel_id.clone());
        applied.channels.remove(channel_id);
    }

    // 3. Everything else through the guard. (No "complete member list" reconciliation: a
    //    removal deletes the channel's memberships, so nothing stale survives a rejoin, and a
    //    page is never proof of a complete list.)
    for c in &batch.channels {
        if fenced_at_or_above(tx, &c.id, c.seq)? {
            continue;
        }
        if guarded_upsert(tx, "channels", &c.id, c.seq, &c.json)? {
            applied.channels.insert(c.id.clone());
        }
    }
    for m in batch.memberships.iter().filter(|m| m.user_id != me) {
        if fenced_at_or_above(tx, &m.channel_id, m.seq)? {
            continue;
        }
        if upsert_membership(tx, m)? {
            applied.channels.insert(m.channel_id.clone());
        }
    }
    for (channel_id, user_id, seq) in &batch.left {
        let changed = tx.execute(
            "UPDATE memberships SET left = 1, seq = ?3
             WHERE channel_id = ?1 AND user_id = ?2 AND seq < ?3",
            params![channel_id, user_id, seq],
        )?;
        if changed == 0 {
            // Not cached (or newer): record the departure's seq, so an older membership row
            // arriving later can't bring the member back.
            tx.execute(
                "INSERT OR IGNORE INTO memberships(channel_id, user_id, seq, left, json)
                 VALUES (?1, ?2, ?3, 1, 'null')",
                params![channel_id, user_id, seq],
            )?;
        } else {
            applied.channels.insert(channel_id.clone());
        }
    }
    for u in &batch.users {
        guarded_upsert(tx, "users", &u.id, u.seq, &u.json)?;
    }

    // 4. Messages (history rows have seq 0).
    for m in &batch.messages {
        if !message_may_land(tx, &m.channel_id, m.seq)? {
            continue;
        }
        // Also replaces a partial row (a tombstone made from a live delete) at the same seq:
        // the server's full row is the truth.
        let changed = tx.execute(
            "INSERT INTO messages(id, channel_id, seq, created_at, json) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET seq = excluded.seq, json = excluded.json,
                 created_at = excluded.created_at
             WHERE excluded.seq > messages.seq
                OR (excluded.seq = messages.seq AND json_extract(messages.json, '$._partial') = 1)",
            params![m.id, m.channel_id, m.seq, m.created_at, m.json.to_string()],
        )?;
        if changed > 0 {
            applied.channels.insert(m.channel_id.clone());
            let deleted = !m.json.get("deleted_at").is_none_or(Value::is_null);
            let body = m.json.get("body").and_then(Value::as_str).unwrap_or("");
            refresh_excerpts(tx, &m.id, if deleted { None } else { Some(body) })?;
        }
    }
    for (id, channel_id, seq) in &batch.tombstones {
        if !message_may_land(tx, channel_id, *seq)? {
            continue;
        }
        // Patch a cached row: its content goes, its author and time stay. Absent: a partial
        // tombstone, so an older version arriving later (an ack, a page, history) can't
        // bring the content back. Either way the server's own tombstone replaces it.
        let patched = tx.execute(
            "UPDATE messages SET seq = ?2,
                 json = json_set(json_remove(json, '$.mentions', '$.mention_everyone', '$.reply_to'),
                                 '$.body', '', '$.seq', ?2, '$.deleted', json('true'),
                                 '$._partial', 1,
                                 '$.reactions', json('[]'), '$.attachments', json('[]'))
             WHERE id = ?1 AND seq < ?2",
            params![id, seq],
        )?;
        if patched == 0 {
            tx.execute(
                "INSERT OR IGNORE INTO messages(id, channel_id, seq, created_at, json)
                 VALUES (?1, ?2, ?3, '', json_object('id', ?1, 'channel_id', ?2, 'body', '',
                         'seq', ?3, 'deleted', json('true'), '_partial', 1,
                         'reactions', json('[]'), 'attachments', json('[]')))",
                params![id, channel_id, seq],
            )?;
        }
        refresh_excerpts(tx, id, None)?;
        applied.channels.insert(channel_id.clone());
    }
    Ok(applied)
}

/// Whether a message row with `seq` may be stored for this channel: the channel is here and
/// not fenced, and the row isn't from before a removal the caller has since come back from
/// (such a row can't be current: anything changed while the caller was away comes back with
/// a higher `seq`, or through history at seq 0).
fn message_may_land(tx: &Transaction<'_>, channel_id: &str, seq: i64) -> rusqlite::Result<bool> {
    if !channel_present(tx, channel_id)? || fenced_at_or_above(tx, channel_id, seq)? {
        return Ok(false);
    }
    Ok(!(seq > 0 && floor_of(tx, channel_id)?.is_some_and(|f| seq <= f)))
}

/// Replies quote their target's body (`reply_to.body`, first 140 characters, "(deleted)" once
/// it's deleted). The server doesn't restamp the replies when the target changes, so the
/// cache follows the target: a deleted message's words never survive in a reply.
fn refresh_excerpts(
    tx: &Transaction<'_>,
    target: &str,
    body: Option<&str>,
) -> rusqlite::Result<()> {
    let excerpt: String = match body {
        Some(b) => b.chars().take(140).collect(),
        None => "(deleted)".to_string(),
    };
    tx.execute(
        "UPDATE messages SET json = json_set(json, '$.reply_to.body', ?2)
         WHERE json_extract(json, '$.reply_to_id') = ?1
           AND json_type(json, '$.reply_to') = 'object'",
        params![target, excerpt],
    )?;
    Ok(())
}

/// The per-row guard: insert, or replace only with a higher `seq`. True if it changed.
fn guarded_upsert(
    tx: &Transaction<'_>,
    table: &str,
    id: &str,
    seq: i64,
    json: &Value,
) -> rusqlite::Result<bool> {
    // `table` is one of our own constants, never input.
    let sql = format!(
        "INSERT INTO {table}(id, seq, json) VALUES (?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET seq = excluded.seq, json = excluded.json
         WHERE excluded.seq > {table}.seq"
    );
    Ok(tx.execute(&sql, params![id, seq, json.to_string()])? > 0)
}

/// A membership through the guard. A departure (`left`) is a row too: only a higher `seq`
/// brings the member back.
fn upsert_membership(tx: &Transaction<'_>, m: &MemberRow) -> rusqlite::Result<bool> {
    Ok(tx.execute(
        "INSERT INTO memberships(channel_id, user_id, seq, left, json) VALUES (?1, ?2, ?3, 0, ?4)
         ON CONFLICT(channel_id, user_id) DO UPDATE SET seq = excluded.seq, left = 0, json = excluded.json
         WHERE excluded.seq > memberships.seq",
        params![m.channel_id, m.user_id, m.seq, m.json.to_string()],
    )? > 0)
}

/// The active removal fence: the caller is out of this channel.
fn fence_of(tx: &Transaction<'_>, channel_id: &str) -> rusqlite::Result<Option<i64>> {
    tx.query_row(
        "SELECT seq FROM removed WHERE channel_id = ?1 AND active = 1",
        [channel_id],
        |r| r.get(0),
    )
    .optional()
}

/// The last removal's seq, active or not: rows from before it are never current.
fn floor_of(tx: &Transaction<'_>, channel_id: &str) -> rusqlite::Result<Option<i64>> {
    tx.query_row(
        "SELECT seq FROM removed WHERE channel_id = ?1",
        [channel_id],
        |r| r.get(0),
    )
    .optional()
}

/// Whether a row with `seq` for this channel is at or below its active removal fence.
fn fenced_at_or_above(tx: &Transaction<'_>, channel_id: &str, seq: i64) -> rusqlite::Result<bool> {
    Ok(fence_of(tx, channel_id)?.is_some_and(|f| seq <= f))
}

fn channel_present(tx: &Transaction<'_>, channel_id: &str) -> rusqlite::Result<bool> {
    Ok(tx
        .query_row("SELECT 1 FROM channels WHERE id = ?1", [channel_id], |_| {
            Ok(())
        })
        .optional()?
        .is_some())
}

/// A scoped removal's rows (spec §7.2): the channel, its members, messages and coverage.
/// Files and Open copies are the files plan's (journalled deletion).
fn remove_channel_rows(tx: &Transaction<'_>, channel_id: &str) -> rusqlite::Result<()> {
    for sql in [
        "DELETE FROM messages WHERE channel_id = ?1",
        "DELETE FROM memberships WHERE channel_id = ?1",
        "DELETE FROM coverage WHERE channel_id = ?1",
        "DELETE FROM channels WHERE id = ?1",
    ] {
        tx.execute(sql, [channel_id])?;
    }
    Ok(())
}
