# Offline cache, outbox and file cache in core (MVP+ #61, #63, #65, #67)

> Status: **approved** (Heavy, two rounds) · 2026-09-25 · Dial: **Heavy** (local data at rest,
> data deletion). Core's side of MVP+. It builds on:
> - the server's sync and idempotent-send spec (#71);
> - the attachments spec (#73, local files, `409 file.already_committed` with `FileOut`);
> - the local data encryption spec (#46: `KeySlot`, missing vs unreadable, the owner's §8).
>
> The apps' UI is #62.

## 1. Goal
An app opens with the last-known channels, members and messages, **with or without a
connection**, and catches up exactly on what it missed. Done means, observably:
- Offline launch shows the channel list and the cached history, and says it's offline.
- Online again, edits, deletes, reactions, membership changes and new messages that happened
  meanwhile appear without re-downloading history.
- A message written offline is pending until sent. It goes in the order written, never
  duplicated, is never silently dropped, and is marked failed with Retry and Delete if refused.
- A file opened once opens again offline. **Keep available offline** keeps a file until unmarked.
- **At rest, only ciphertext:** the stores, their journals, download partials, upload snapshots
  and leftovers of a crash.
  - Stated exceptions: a file the user **Saves**, written where they chose; and the copy
    **Open** hands to another app (§6.4), deleted at quit, at the next launch and by startup
    reconciliation.
- Wipes follow #46 §8 and §7 below, with crypto-erase **per store**: a wiped store's key is
  destroyed, not just its ciphertext (per-file deletion is best-effort, §3).

## 2. Not doing
- Search in the cache; eviction of messages (history is kept once fetched; files are evicted,
  §6.5); migrations (pre-1.0: a different format version rebuilds the **cache**, and the outbox
  is handled per §5.6).

## 3. Storage and keys
- **Two stores per (server, user),** each a SQLite database through `rusqlite` with
  **SQLCipher** (`bundled-sqlcipher`: CommonCrypto on Apple, libcrypto on Linux):
  - **cache.db:** synced state, rebuildable at any time;
  - **outbox.db:** unsent messages and attachment snapshots, never rebuilt away (§5.6).
  Settings on open:
  - `PRAGMA cipher_memory_security = ON`;
  - `PRAGMA temp_store = MEMORY`, so no plaintext spill files;
  - `journal_mode = WAL`. SQLCipher encrypts WAL pages; `-shm` holds only the index. Both are
    verified by test.
  **Decision worth arguing:** SQLCipher, over per-row AES-GCM on plain SQLite. Per-row
  encryption avoids libcrypto on Linux but leaves ids, times and counts readable, and indexes
  work only on cleartext columns.
- **Keys are random, and a store's key is independently destroyable** (crypto-erase per store):
  - each store has its own random 256-bit key, held in the `KeySlot` under a named slot
    (`cache:<store id>`, `outbox:<store id>`; #46 §3a takes a slot name);
  - destroying a store's slot makes that store, its WAL, its files and any copy or snapshot of
    them undecryptable. Nothing is derived from a device key that could re-derive it;
  - each cached file and outbox snapshot has its own random key (for the nonce scheme, §6.1),
    stored in its row inside the encrypted database.
  **Per-file deletion is best-effort, not crypto-erase:** a deleted file's key can survive in
  SQLite free pages, the WAL, or an older copy of the database, and anyone holding that copy
  *and* the live store key could still decrypt the file. The crypto-erase guarantee is per
  store, and every wipe in §7.1 destroys whole stores.
- **Location:**
  - the app's data directory (the Mac container's `Application Support/Brook/stores/`;
    `$XDG_DATA_HOME/brook/stores/` on Linux);
  - one directory per store id, where the id is a random UUID mapped from (origin, user id) in
    a small encrypted index, so no handle or server name appears in paths;
  - the Mac excludes the directory from backups.
- **cache.db schema v1:**
  - `meta(format, cursor, generation)`;
  - `channels(id PK, seq, json)`;
  - `removed(channel_id PK, seq)`;
  - `memberships(channel_id, user_id, seq, json)`;
  - `users(id PK, seq, json)`;
  - `messages(id PK, channel_id, seq, created_at, json)`;
  - `coverage(channel_id PK, newest_id, oldest_id, complete_to_start bool)`;
  - `files(file_id PK, sha256, size, key, state, pinned, last_opened)`;
  - `deletions(path PK)`, the deletion journal (§7.3).
- **outbox.db schema v1:**
  - `meta(format)`;
  - `outbox(ordinal INTEGER PK AUTOINCREMENT, client_id UNIQUE, channel_id, body, state, attempts, error, created_at)`;
  - `outbox_files(client_id, file_client_id UNIQUE, snapshot_path, sha256, size, content_type, filename, file_id, key)`.

## 4. Sync
### 4.1 Ordering rules (one function applies every row, whatever its source)
- **Per-row guard:** an incoming row replaces the stored one only if its `seq` is higher. This
  applies to `/sync` pages, live WebSocket events, history pages and send acknowledgements
  (§5.3).
- **Removal fence:**
  - `removed_channels` writes `removed(channel_id, seq)` and deletes the channel's rows (§7.3
    for its files);
  - after that, a row for that channel applies only if its `seq` is higher than the removal's;
  - a rejoin is a membership row with a higher `seq`: it clears the fence;
  - a removal older than the stored channel or membership `seq` is ignored.
  So a stale page can't remove a rejoined channel, and a late event or history page can't
  resurrect a removed one.
- **History pages carry no `seq`.** A page applies only if the channel is still present and
  un-fenced when it arrives. Its rows go in with `seq = 0`, so any real row wins.
- **The cursor advances only from `/sync`,** in the same transaction as that page's rows. A
  crash mid-page leaves the old cursor, and replay is idempotent.
### 4.2 When
- After every (re)connect: `/sync` until `more` is false. Then live events.
- A periodic `/sync` every few minutes catches anything a dropped event missed.
- The **first sync** (`since=0`, or after `410 sync.reset` → the cache is rebuilt, §7) is
  **state only** (#71): channels, members and users.
### 4.3 History coverage
- Per channel, `coverage` records the contiguous range the cache holds:
  - `newest_id` / `oldest_id`;
  - `complete_to_start`, for when a page came back short.
- **Opening a channel with no coverage:**
  1. fetch the newest page (no `before`);
  2. merge it (a live message that arrived earlier is inside or after it);
  3. set the range.
- Scrolling back pages `before = oldest_id` and extends the range down.
- `/sync` and live messages extend the top. A message older than `newest_id` inside the range is
  an edit or late delivery, and the guard handles it.
- A message outside the range (e.g. arrived before the first head fetch) is kept, but never
  treated as proof of coverage.

## 5. Outbox (#63)
1. **Queued means durable.** A send commits the outbox row, and for attachments their
   encrypted snapshots, **before** it is shown as pending:
   - `client_id` is a UUIDv4;
   - `ordinal` is a monotonic enqueue order, not a timestamp;
   - each attachment is copied into the outbox store as an encrypted snapshot, with its own
     `file_client_id` and `sha256`, so later edits, moves or deletion of the source file
     change nothing.
2. **Sender:** one per channel, by `ordinal`. A failed row blocks later rows of that channel
   only. On startup, rows left `sending` by a crash go back to `pending` and are re-sent with
   the same `client_id` (idempotent).
3. **Acknowledgement (200 or 201).** Two databases, so no single transaction; the order makes
   a crash safe:
   1. apply the returned message to cache.db through §4.1's guard (a later edit or tombstone
      already there wins) and commit;
   2. then delete the outbox row whose `client_id` equals the **echoed** `client_id`.
   A crash between the two leaves the row; on restart it is re-sent with the same `client_id`,
   the server returns the stored message (200), step 1 re-applies idempotently, and step 2
   runs.
   An echo that doesn't match is a bug: the row is marked failed and never auto-resent.
4. **Attachments:**
   - create each file with its `file_client_id` (#73);
   - PUT the decrypted snapshot, streamed;
   - `409 file.already_committed`: compare `details.sha256` with the snapshot's. Equal means
     done; different means failed.
   - then send the message referencing the `file_id`s. Snapshots are deleted after the ack.
5. **Retries, Delete and Retry:**
   - network errors retry with backoff and never fail the row;
   - a 4xx marks it **failed** with the server's code;
   - Retry and Delete are serialised with the sender;
   - Delete of a row in flight waits for that attempt. If it was accepted, the message
     exists and the UI shows it as sent (a send can't be revoked).
6. **Rebuilds never drop sends.**
   - A cache rebuild (`410`, a cache format change) leaves outbox.db untouched.
   - An outbox format change, a missing key, sign-out with "Remove this device's data", or a
     different user signing in **would** delete unsent messages. The app says so first ("2
     messages haven't been sent and will be deleted"), and for sign-out the user can cancel.
7. **When the outbox can't be used, order still holds:**
   - **Unreadable** (the key is locked or not answering): its pending order is unknown, not
     empty, so **sending is disabled** until it can be read ("Sending resumes when Brook can
     read its storage"). Reading still works.
   - **Readable but not writable** (disk full): a new message is sent straight to the server,
     not shown as queued, **only in a channel with no pending or failed rows**. Elsewhere it's
     refused with the reason, so it can never overtake an earlier message.

## 6. Files (#65, #67)
### 6.1 Format
- Each file has its own random 256-bit key, stored in its cache.db row.
- It's encrypted in 1 MiB chunks: AES-256-GCM with nonce = the chunk index (96-bit big-endian
  counter), which is unique because the key is unique to the file. AAD = `file_id | index |
  last-chunk flag`.
- A file is complete only when its last chunk (flagged) is written and the plaintext `sha256`
  matches `FileOut.sha256`.
- A rewrite never reuses a key: a changed download starts a new key and a new file.
### 6.2 Downloads
- Written **already encrypted**: each received MiB is sealed and appended. A partial is
  ciphertext, recorded in `files.state = partial` with its last complete chunk.
- Resume uses `Range` from that chunk's end, with `If-Range: <sha256>`. If the server's file
  changed, the partial is discarded and a new key is used.
### 6.3 Uploads
- Uploads stream from the outbox snapshot (decrypted in memory, chunk by chunk), with a
  per-transfer timeout. The client-wide 30 s doesn't apply.
- A 401 before the PUT starts refreshes, then re-opens the snapshot.
### 6.4 Open and Save
- **Save** decrypts straight into the user's chosen location.
- **Open** decrypts into the app's temporary directory, because another app needs a file. That
  copy is deleted:
  - when the app quits;
  - at the next launch;
  - by startup reconciliation (§7.3).
  This is the one plaintext copy the app makes on its own, and §1 states it. A later option
  could skip Open for sensitive files.
### 6.5 Pinning and eviction
- Pinned files are downloaded at once and never evicted. Others are evicted by LRU above a cap
  (default 1 GB per store).

## 7. Wipes
### 7.1 Triggers
| Trigger | Effect |
|---|---|
| `410 sync.reset`, or a cache format change | cache.db rebuilt: new cache key, old slot destroyed; outbox kept |
| Outbox format change | unsent messages surfaced (§5.6), then outbox.db rebuilt |
| A store's key is **missing** | that store rebuilt (cache) or surfaced then rebuilt (outbox) |
| A key is **unreadable** | nothing deleted; online-only (§5.7) until it can be read |
| **A different user signs in** (owner, #46 §8) | the other users' stores on this device are wiped, after surfacing their unsent messages |
| Sign-out with "Remove this device's data" (ticked by default) | this user's stores and files wiped, **locally and first**, whether or not the logout request succeeds |
| A channel is removed | §4.1 |
### 7.2 Quiesce, then erase (whole-store wipes only)
A **store wipe** (every row of §7.1 except channel removal):
1. bumps the store's `generation` (in memory and in `meta`);
2. cancels in-flight downloads, uploads, history requests and the sender for that store or
   channel, and waits for them to stop;
3. closes database handles;
4. destroys the key slot (crypto-erase) **before** deleting files.
5. deletes that store's Open copies (§6.4; the app keeps them in a per-store temp directory)
   **before** reporting the wipe done, whether the app then keeps running or not.
Every operation checks the generation it started under before writing, so nothing recreates
wiped data.

**Channel removal is scoped**, never a store wipe:
- in one cache.db transaction: write the fence, delete the channel's rows, and journal its
  files (§7.3);
- cancel that channel's history requests and downloads;
- delete its Open copies;
- mark its outbox rows failed with the server's code (the user sees them, and can Delete);
- other channels, the store key and other unsent messages are untouched.
### 7.3 Crash-safe deletion
- File deletions are journalled in `deletions` inside the transaction that removes their rows.
  The unlink happens after the commit, and the journal entry is cleared after the unlink.
- **Startup reconciliation** runs per store, and **only after that store opened with its key
  and its tables were read successfully**. With an unreadable key, nothing of that store is
  classified as an orphan or deleted. Before anything else, it:
  - finish journalled deletions;
  - delete files in the store directory with no row;
  - delete download partials of files no longer wanted;
  - delete upload snapshots with no outbox row;
  - delete everything in the Open temp directory.

## 8. Core API (FFI)
- **Reading:**
  - `cached_channels()`;
  - `cached_messages(channel, before?, limit)`, which pages from the network and extends
    coverage when needed;
  - `pending_messages(channel)`;
  - `cache_state()`: online/offline, syncing, last synced, and whether offline storage is
    available.
- **Change notices:** a `CacheEvent` stream (which channel changed); the UI re-reads.
- **Sending:** `send_message(channel, body, attachment_paths)` → `client_id`, durable before it
  returns (§5.1); `retry_send` and `delete_pending`.
- **Files:** `open_file(file_id)` → temp path; `save_file(file_id, destination)`; `pin_file` and
  `unpin_file`.
- **Sign-out:** `logout(remove_data: bool)`, and `unsent_count()` for the sign-out warning.

## 9. Tests (each seen failing under a named mutation)
- **At rest:**
  - no plaintext bytes (a sentinel string in a message or file) anywhere under the store
    directory, including `-wal`, `-shm` and partials: after writes, after a simulated crash
    mid-download, and mid-transaction;
  - a store opens only with its own key;
  - destroying a slot makes a copied store file unreadable.
- **File format:**
  - nonce and AAD per chunk;
  - a truncated or reordered chunk fails;
  - resume after a crash continues from the last complete chunk without reusing a nonce under
    a different plaintext;
  - a changed server file discards the partial and gets a new key.
- **Ordering:**
  - a stale page after a live event keeps the live row;
  - a stale removal after a rejoin keeps the channel;
  - a late event or history page after a removal doesn't resurrect it;
  - an ack after a tombstone keeps the tombstone;
  - page replay after a crash is idempotent.
- **Coverage:**
  - a live message into an empty channel, then open: the head page is fetched and there's no
    gap;
  - a short page sets `complete_to_start`.
- **Outbox:**
  - durable before pending (kill between the two);
  - a lost response and a resend make one message;
  - equal timestamps keep enqueue order;
  - a crash while `sending` resumes with the same `client_id`;
  - Delete during an in-flight accepted send shows it as sent;
  - an echo mismatch fails the row;
  - a source file edited after enqueue uploads the snapshot's bytes;
  - `already_committed` with an equal and with a different `sha256`;
  - `410` keeps the outbox;
  - sign-out surfaces the unsent count.
- **Wipes:**
  - each row of §7.1;
  - a channel removal leaves other channels' data and other unsent messages intact, and fails
    only that channel's outbox rows;
  - a sign-out wipe deletes the Open copies while the app keeps running;
  - an unreadable-key startup with real cached files and snapshots deletes none of them;
  - a crash between the two acknowledgement commits: one message, and the outbox row cleared;
  - an outbox that is unreadable blocks sending; an unwritable one sends directly only where
    nothing is pending;
  - a deleted file stays decryptable from an earlier database copy while the store key lives
    (documents the limit), and not after the store is wiped (the guarantee);
  - an unreadable key never deletes;
  - a wipe during a download leaves nothing behind;
  - a crash between commit and unlink is finished at the next startup;
  - sign-out deletion completes with the server unreachable.
- **Live (itest):**
  - offline read, a queued message, reconnect, and exactly one send;
  - an edit made elsewhere while offline appears after reconnect.

## 10. Where this fails
| Failure | Response |
|---|---|
| SQLCipher adds libcrypto on Linux | the Flatpak runtime ships it; the tarball's INSTALL lists it (Linux client's call) |
| A long offline period past tombstone retention | `410` rebuilds the cache; the outbox is kept |
| The same account edits on two offline devices | the server orders by commit; each cache follows `seq` |
| Disk full | the transaction fails and nothing is half-written; the app goes online-only and says so |
| Another app keeps the Open copy open after the app quits | it's deleted at the next launch or reconciliation |

## 11. Review log
**Round 1 — Codex + Vibe (Heavy).** All accepted. Changes:
- Stores and files get random, independently destroyable keys (real crypto-erase, not
  deletion of re-derivable ciphertext), and the `KeySlot` gains named slots (#46 §3a).
- Encryption holds before the first disk write: encrypted download partials, memory-only
  SQLite temp storage, WAL/shm checked, and the Open copy stated as the one exception, with
  launch-time cleanup.
- A chunk format with a per-file key and counter nonces, and authenticated position and last
  chunk.
- A separate outbox store that survives rebuilds, with its loss surfaced and never silent;
  encrypted attachment snapshots at enqueue; a monotonic ordinal; crash recovery of `sending`
  rows; Delete and Retry serialised; an echo mismatch fails the row.
- A removal fence; history pages guarded; history coverage tracking; acks through the `seq`
  guard.
- Quiesce by store generation before a wipe; a deletion journal and startup reconciliation;
  sign-out's local deletion independent of the network.
- The account-switch rule follows #46 §8.
- The test list covers the interleavings.

**Round 2 — Codex + Vibe.** Vibe: none. Codex raised six new points, all accepted, with no round 3
and nothing disputed:
- per-file crypto-erase isn't claimed (the guarantee is per store, and per-file deletion is
  best-effort);
- channel removal is scoped and never a store wipe;
- wipes delete the store's Open copies before reporting done;
- a crash-safe two-database acknowledgement order;
- an unusable outbox never lets a send overtake queued ones;
- startup reconciliation only after a successful unlock and read.
Each has a test in §9. The gate closes.
