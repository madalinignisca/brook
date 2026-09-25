# Sync (offline cache catch-up) and idempotent sends: server design

Status: draft for Heavy review. Issues #60 (forward sync) and #63 (outbox, server
piece). The cache and outbox themselves are core's; this is the wire they rely on.

## 1. Problem

A client that was offline must fetch **exactly what it missed**, without re-downloading
history. The missed changes are:
- new messages;
- edits, deletes and reaction changes on messages it already has;
- channels created, renamed, archived or left;
- members joining and leaving;
- other devices' read state;
- profile changes.

`after=` on `GET /channels/{id}/messages` (already implemented) returns only new
messages in one channel, so it isn't enough.

A message sent from the outbox may be retried after a lost response, so it must never be
stored twice.

## 2. The change sequence (commit order = sequence order)

Every write that the cache must learn about stamps the rows it changes with a **change
sequence number** (`seq`, bigint). The number is drawn from one counter row:

```sql
UPDATE sync_counter SET seq = seq + 1 WHERE id = 1 RETURNING seq
```

It's done **once per transaction, just before commit**. All rows the transaction changed
get that one value.

**Why a counter row and not a Postgres sequence:** a sequence hands out numbers at
`nextval` time, not at commit. Take T1 with 104, still open, and T2 with 105, which
commits first. A sync then returns 105 and `next=105`; T1 commits 104, and no later sync
ever returns it: a change lost for good. The counter row's lock is held until commit, so
change-writing transactions commit one at a time, in `seq` order. At family scale that
serialisation costs nothing. It works identically on SQLite, which serialises writers
anyway, and it is tested on Postgres with an interleaving test (§6).

Stamped rows:

| Row | When |
|---|---|
| `messages.seq` | Create, edit, delete (tombstone), and any reaction change (the whole reaction aggregate travels with the message). |
| `channels.seq` | Create, rename or topic change, archive, visibility. |
| `memberships.seq` | Join or add, last-read change, and a removal (see tombstones). |
| `users.seq` | `display_name` or status change. It's delivered to users who share a channel. |

**Cost note:** `last_read` changes take the counter lock too, so every read-marker update
serialises with message writes. That is fine at family scale, and it's the first place to
look if write latency ever shows up.

**Tombstones:**
- A deleted message keeps its row with `deleted_at`, an empty body and no reactions (and
  later no attachments), and it is stamped.
- A removed membership becomes a row in `sync_tombstones(kind, channel_id, user_id, seq)`,
  so both the removed user (`removed_channels`) and the remaining members (member left)
  learn about it.
- **A hard channel delete**, if one ever exists, writes a `removed_channels` tombstone for
  **every** member. It must never just vanish. Archive is already a stamped row change.
- Tombstones are kept indefinitely pre-1.0 (small). Any later pruning must raise
  `sync_floor`, see §3.

## 3. `GET /api/v1/sync?since=<cursor>&limit=500`

```
200 {
  "channels":          [ChannelOut + seq],          // channels I'm in, changed since
  "removed_channels":  [{channel_id, seq}],         // I left or was removed
  "memberships":       [{channel_id, user_id, role, last_read_message_id?, seq}],
  "left_members":      [{channel_id, user_id, seq}],
  "users":             [{id, handle, display_name, status, seq}],  // people I share a channel with
  "messages":          [MessageOut + seq],          // incl. edited/deleted, reactions
  "next": "<cursor>",
  "more": bool
}
```

- **Scope:** only channels the caller is a member of **now**, plus `removed_channels` for
  ones they are no longer in (so the cache drops them). `last_read_message_id` appears only
  on the caller's own membership rows.
- **Cursor:** opaque to the client. Internally it's the last `seq` returned. The first sync
  is `since=0`. Pages never split a `seq` (a transaction's rows arrive together), so a page
  may slightly exceed `limit`. `more: true` means ask again with `next`.
- **Reset:** `410 {code: "sync.reset"}` when `since` is below `sync_floor` (tombstones
  pruned) or above the server's current `seq` (a database restored from backup, a wiped
  test server). The client wipes its cache and syncs from `0`. Pre-1.0 this is the entire
  client migration story.
- **Initial sync is state only.** `since=0`, and the first sync after a `410 sync.reset`,
  returns channels, memberships and users, **no messages**, and a cursor at the current
  `seq`. The client pages messages per channel with `before=` as it needs them. From then
  on `/sync` delivers message changes after that cursor. That is coherent: anything older
  than the cursor comes from paging, anything newer from sync, and the per-row `seq` rule
  makes the overlap harmless. Downloading all history on a phone is exactly what §1 set
  out to avoid.
- **A channel new to me arrives complete.** When the page contains the caller's **own**
  membership row for a channel (joined, re-added, or a DM someone opened with me), the
  response also includes that channel's **full current member list** in `memberships`, and
  every one of those members in `users`, **regardless of `seq`**. Otherwise the older
  members' rows, stamped before my cursor, would never arrive, and a non-admin can't call
  `/users` to fill in the names.
- **Channels added later: history is not included.** A channel I'm added to, or re-added
  to, has history that predates my cursor. The client back-fills it with `before=` paging
  as today. This is deliberate: do not "fix" it by dumping history into `/sync`.
- **The cursor stays opaque** in PROTOCOL.md too (it's a string to clients), so a
  composite cursor later is free.
- **Per-row `seq`:** every returned row carries its `seq`. WebSocket events for the same
  rows (`message.created`, `.updated`, `.deleted`, `reaction.*`, `channel.*`, `member.*`)
  carry it too. The client keeps the highest `seq` per row and ignores older data, so a
  `/sync` page computed a moment before a live event can't overwrite it. The client's
  cursor only advances from `/sync`, never from live events.

## 4. Idempotent send (#63)

`POST /channels/{id}/messages` gains an optional `client_id` (UUID, generated by the
client).
- It's unique per `(author_id, client_id)`, as a partial unique index where `client_id` is
  not null.
- A retry with a `client_id` already stored for this author returns the **stored**
  message, `200` instead of `201`, unchanged, **even if the body differs** (it's the same
  outbox entry; the client trusts the stored one). Never a 409, never a duplicate.
- `client_id` is echoed in the POST response and in the `message.created` WebSocket event,
  so the sender's cache matches the pending outbox entry to the stored message without a
  race.
- A concurrent double submit is resolved by the unique index: the loser catches the
  conflict and returns the winner's row.

## 5. Migration

- A `seq bigint not null default 0` column on `messages`, `channels`, `memberships` and
  `users`.
- The `sync_counter` table (one row), `sync_tombstones`, and `sync_floor` (a column on
  the counter row).
- `messages.client_id` plus its partial unique index.
- **Backfill:** existing rows get `seq = 1` and the counter starts at `1`. A client's
  first sync (`since=0`) is still state only (§3): it gets current channels, memberships and
  users, and a cursor at the current `seq`; messages come from `before=` paging.

## 6. Tests (each seen red under mutation)

1. **The lost-change interleaving (Postgres).** Hold one change-writing transaction open
   after it takes the counter, and commit a second. A sync in between must not return a
   cursor past the open one, and after both commit, a sync from that cursor returns the
   first transaction's change. It fails with a plain sequence.
2. Every stamped operation bumps `seq`: create, edit, delete, reaction, rename, archive,
   add or remove member, read change, display-name change.
3. Scope: nothing from channels I'm not in; `removed_channels` after removal; other users'
   `last_read` never leaks.
3a. Added to a channel with existing members: the first `/sync` returns every member and
    their display names.
3b. `since=0` returns no messages and a cursor at the current `seq`; a message after that
    arrives on the next sync.
4. `410 sync.reset` for a cursor above the maximum.
5. Paging never splits a `seq`; `more`/`next` walk to the end.
6. `client_id`: a retry returns the stored message with 200, a different body is ignored,
   and a concurrent double submit stores one row (Postgres); the echo appears in the POST
   response and the WS event.
7. The WebSocket events carry `seq`.
