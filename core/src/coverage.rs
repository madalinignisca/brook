//! History coverage (plan C2, spec §4.3): per channel, the contiguous range of messages the
//! cache holds. A cached message outside that range (a live one that arrived before the
//! channel was ever opened) is kept, but it is never proof that the history around it is
//! there. Message ids are UUIDv7, so they sort by time.

use rusqlite::{params, OptionalExtension, Transaction};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Range {
    /// Both `None` for a channel covered and empty.
    pub(crate) newest_id: Option<String>,
    pub(crate) oldest_id: Option<String>,
    /// A page came back short: nothing older exists on the server.
    pub(crate) complete_to_start: bool,
}

pub(crate) fn range(tx: &Transaction<'_>, channel_id: &str) -> rusqlite::Result<Option<Range>> {
    tx.query_row(
        "SELECT newest_id, oldest_id, complete_to_start FROM coverage WHERE channel_id = ?1",
        [channel_id],
        |r| {
            Ok(Range {
                newest_id: r.get(0)?,
                oldest_id: r.get(1)?,
                complete_to_start: r.get(2)?,
            })
        },
    )
    .optional()
}

/// The newest page (no `before`) was fetched and merged: it defines the range. `ids` are the
/// page's message ids; fewer than `limit` means the channel's whole history is in it.
pub(crate) fn record_head(
    tx: &Transaction<'_>,
    channel_id: &str,
    ids: &[String],
    limit: usize,
) -> rusqlite::Result<()> {
    let complete = ids.len() < limit;
    let (Some(newest), Some(oldest)) = (ids.iter().max(), ids.iter().min()) else {
        // An empty channel: covered, and complete.
        return tx
            .execute(
                // A stale empty answer never marks a held, non-empty range complete.
                "INSERT INTO coverage(channel_id, newest_id, oldest_id, complete_to_start)
                 VALUES (?1, NULL, NULL, 1)
                 ON CONFLICT(channel_id) DO UPDATE SET complete_to_start = 1
                 WHERE coverage.newest_id IS NULL",
                [channel_id],
            )
            .map(|_| ());
    };
    // A range we already hold may reach further down: keep the lower bottom, but only if the
    // two ranges overlap. A head page wholly older than the held range (a delayed fetch)
    // doesn't touch it.
    let existing = range(tx, channel_id)?;
    if let Some(Range {
        oldest_id: Some(held_oldest),
        ..
    }) = &existing
    {
        if newest.as_str() < held_oldest.as_str() {
            return Ok(());
        }
    }
    let (oldest, complete) = match existing {
        Some(Range {
            newest_id: Some(held_newest),
            oldest_id: Some(held_oldest),
            complete_to_start,
        }) if held_newest.as_str() >= oldest.as_str()
            && newest.as_str() >= held_oldest.as_str() =>
        {
            (
                held_oldest.min(oldest.clone()),
                complete_to_start || complete,
            )
        }
        _ => (oldest.clone(), complete),
    };
    tx.execute(
        "INSERT INTO coverage(channel_id, newest_id, oldest_id, complete_to_start)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(channel_id) DO UPDATE SET newest_id = max(coalesce(newest_id, ''), ?2),
             oldest_id = ?3, complete_to_start = ?4",
        params![channel_id, newest, oldest, complete],
    )?;
    Ok(())
}

/// An older page (`before = oldest_id`) was merged: extend the range down. A page fetched
/// with any other `before` isn't contiguous with the range and doesn't extend it.
pub(crate) fn record_older(
    tx: &Transaction<'_>,
    channel_id: &str,
    before: &str,
    ids: &[String],
    limit: usize,
) -> rusqlite::Result<()> {
    let Some(r) = range(tx, channel_id)? else {
        return Ok(()); // no range to extend: the head fetch sets it
    };
    if r.oldest_id.as_deref() != Some(before) {
        return Ok(());
    }
    let oldest = ids.iter().min().cloned().or(r.oldest_id);
    tx.execute(
        "UPDATE coverage SET oldest_id = ?2, complete_to_start = ?3 WHERE channel_id = ?1",
        params![channel_id, oldest, r.complete_to_start || ids.len() < limit],
    )?;
    Ok(())
}

/// A sync run just caught up (its last page is in this transaction): every message created
/// since the last sync is now cached, whatever order the pages delivered them in (they come
/// by latest `seq`, not by creation). So each covered channel's range reaches its newest
/// cached message. Live messages alone never move the top: one could have been missed.
///
/// Only rows the run's snapshot accounts for count: `0 < seq <= cursor`. A live message
/// newer than the snapshot (it may follow one not yet delivered) and history rows (seq 0,
/// possibly outside the range) don't.
pub(crate) fn settle_tops(tx: &Transaction<'_>, cursor: i64) -> rusqlite::Result<()> {
    tx.execute(
        "UPDATE coverage SET
             newest_id = (SELECT max(id) FROM messages
                          WHERE channel_id = coverage.channel_id AND seq > 0 AND seq <= ?1),
             oldest_id = coalesce(oldest_id,
                                  (SELECT min(id) FROM messages
                                   WHERE channel_id = coverage.channel_id AND seq > 0 AND seq <= ?1))
         WHERE (SELECT max(id) FROM messages
                WHERE channel_id = coverage.channel_id AND seq > 0 AND seq <= ?1)
               > coalesce(newest_id, '')",
        [cursor],
    )?;
    Ok(())
}
