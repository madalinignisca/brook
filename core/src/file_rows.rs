//! Which cached files belong to which cached message, kept in the cache's own transactions
//! (keep-offline spec §4, §6; plan step 2).
//!
//! `message_files` indexes every file a cached message lists. When a message stops listing a
//! file (it was deleted from the message, the message became a tombstone, its channel was
//! removed, or the sync was reset), the file's `files` row goes in the same transaction and
//! its blob is journalled in `deletions`; `Files::sweep_journal` unlinks it after the commit.
//! So every cached blob belongs to a message the cache tracks, and nothing can outlive the
//! message that made it.

use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::Value;

/// The blob of `file_id`, relative to the store directory (the journal's path form).
pub(crate) fn blob_path(file_id: &str) -> String {
    format!("files/{file_id}")
}

/// The file ids a message's JSON lists (`attachments[].id`), in order.
pub(crate) fn listed(json: &Value) -> Vec<String> {
    json.get("attachments")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter_map(|f| f.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Index the stored version of a message. Files it listed before and no longer lists are
/// dropped (a list only shrinks). Returns the dropped ids.
pub(crate) fn index_message(
    tx: &Transaction<'_>,
    message_id: &str,
    channel_id: &str,
    now: &[String],
) -> rusqlite::Result<Vec<String>> {
    let before = ids(
        tx,
        "SELECT file_id FROM message_files WHERE message_id = ?1",
        message_id,
    )?;
    let gone: Vec<String> = before.into_iter().filter(|f| !now.contains(f)).collect();
    tx.execute(
        "DELETE FROM message_files WHERE message_id = ?1",
        [message_id],
    )?;
    for file_id in now {
        tx.execute(
            "INSERT OR REPLACE INTO message_files(file_id, message_id, channel_id)
             VALUES (?1, ?2, ?3)",
            params![file_id, message_id, channel_id],
        )?;
    }
    drop_files(tx, &gone)
}

/// A message is gone or a tombstone: all its files go.
pub(crate) fn drop_message(
    tx: &Transaction<'_>,
    message_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let gone = ids(
        tx,
        "SELECT file_id FROM message_files WHERE message_id = ?1",
        message_id,
    )?;
    tx.execute(
        "DELETE FROM message_files WHERE message_id = ?1",
        [message_id],
    )?;
    drop_files(tx, &gone)
}

/// A channel left the cache (the caller was removed from it): its files go.
pub(crate) fn drop_channel(
    tx: &Transaction<'_>,
    channel_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let gone = ids(
        tx,
        "SELECT file_id FROM message_files WHERE channel_id = ?1",
        channel_id,
    )?;
    tx.execute(
        "DELETE FROM message_files WHERE channel_id = ?1",
        [channel_id],
    )?;
    drop_files(tx, &gone)
}

/// The sync was reset (`410`): the old server's file ids mean nothing, so every file goes,
/// pinned or not.
pub(crate) fn drop_all(tx: &Transaction<'_>) -> rusqlite::Result<Vec<String>> {
    let mut gone = all(tx, "SELECT file_id FROM message_files")?;
    for f in all(tx, "SELECT file_id FROM files")? {
        if !gone.contains(&f) {
            gone.push(f);
        }
    }
    tx.execute("DELETE FROM message_files", [])?;
    drop_files(tx, &gone)
}

/// Drop these files' cache rows and journal their blobs. Returns the ids given (whether or
/// not a blob was cached: the UI shows every one as gone).
pub(crate) fn drop_files(
    tx: &Transaction<'_>,
    file_ids: &[String],
) -> rusqlite::Result<Vec<String>> {
    for file_id in file_ids {
        let cached: Option<i64> = tx
            .query_row("SELECT 1 FROM files WHERE file_id = ?1", [file_id], |r| {
                r.get(0)
            })
            .optional()?;
        if cached.is_some() {
            tx.execute("DELETE FROM files WHERE file_id = ?1", [file_id])?;
            tx.execute(
                "INSERT OR IGNORE INTO deletions(path) VALUES (?1)",
                [blob_path(file_id)],
            )?;
        }
    }
    Ok(file_ids.to_vec())
}

fn ids(tx: &Transaction<'_>, sql: &str, key: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = tx.prepare(sql)?;
    let rows = stmt.query_map([key], |r| r.get(0))?;
    rows.collect()
}

fn all(tx: &Transaction<'_>, sql: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = tx.prepare(sql)?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    rows.collect()
}
