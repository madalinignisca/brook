# Offline cache, outbox and file cache in core (MVP+ #61, #63, #65, #67)

> Status: **draft for review** · 2026-09-25 · Dial: **Heavy** (local data at rest, data
> deletion). Core's side of MVP+. It builds on:
> - the server's sync and idempotent-send spec (#71);
> - the attachments spec (#73, local files);
> - the local data encryption spec (#46: the per-device key and the `KeySlot` trait).
>
> The apps' UI for it is #62 (macOS and GTK), specified with them.

## 1. Goal
An app opens with the last-known channels, members and messages, **with or without a
connection**, and catches up exactly on what it missed when the connection returns. Done means,
observably:
- Offline launch shows the channel list and recent history, as last seen, and says it's offline.
- Online again, edits, deletes, reactions, membership changes and new messages that happened
  meanwhile appear without re-downloading history (`/sync` from a stored cursor).
- A message written offline is shown as pending, sent in order when the connection returns,
  never duplicated (`client_id`), and marked failed with a retry if the server refuses it.
- A file opened once opens again offline. A file marked **Keep available offline** is downloaded
  and kept until unmarked.
- Nothing readable is left on disk. The cache is encrypted with the device key (#46), and wiped
  by a cursor reset (`410 sync.reset`), a lost key, a different user signing in, or a sign-out with
  "Remove this device's data" ticked (the owner's §8 decision).

## 2. Not doing
- Full-text search in the cache (later; a server feature first).
- Eviction of messages (history is kept once fetched; attachments are evicted, §6).
- Migrations: pre-1.0 the store has a format version, and a different version is deleted and
  rebuilt (owner rule).
- Sharing one store between apps or users: one store per (server, user) on a device.

## 3. Storage
- **SQLite through `rusqlite` with SQLCipher** (the `bundled-sqlcipher` feature: CommonCrypto on
  Apple, libcrypto on Linux). The whole file is encrypted, with a 256-bit key; nothing is
  readable, not even ids, times or counts. **Decision worth arguing:** per-row AES-GCM over plain
  SQLite would avoid the libcrypto dependency on Linux, but leaves the metadata (which channels,
  how many messages, when) readable in the file, and indexes would only work on cleartext
  columns.
- **Key:** `HKDF-SHA256(device_key, info = "brook.cache.v1" | server origin | user id)`, where
  the device key comes from #46's `KeySlot`. So each (server, user) store has its own key, and
  crypto-erase of one store is deleting its file (its key can be re-derived, but the ciphertext
  is gone).
- **Location:** the app's data directory (the Mac's container
  `Application Support/Brook/cache/`, Linux `$XDG_DATA_HOME/brook/cache/`), with one file
  `<hash(origin|user id)>.db`, never the handle or server name in cleartext. The Mac excludes the
  directory from backups; the key is ThisDeviceOnly anyway.
- **Schema v1:**
  - `meta(format, cursor, synced_at)`;
  - `channels(id PK, seq, json)`;
  - `memberships(channel_id, user_id, seq, json, PK(channel_id, user_id))`;
  - `users(id PK, seq, json)`;
  - `messages(id PK, channel_id, seq, created_at, json)`, indexed by `(channel_id, created_at)`;
  - `outbox(client_id PK, channel_id, body, attachments json, state, attempts, created_at, error)`;
  - `files(file_id PK, sha256, size, path, pinned, last_opened)`.
  Rows keep the server's JSON (`json` column) plus the columns needed to query and order.

## 4. Sync engine
- **Every row applied is `seq`-guarded:** an incoming row replaces the stored one only if its
  `seq` is higher. That holds for `/sync` pages and live WebSocket events alike, so a page
  computed just before a live event can't overwrite it (#71 §3).
- **The cursor advances only from `/sync`**, stored in the same transaction as the page's rows.
  A crash mid-page leaves the old cursor, and the rows re-apply idempotently.
- **When:**
  - after every (re)connect, `/sync` pages until `more` is false;
  - then live events keep the cache current;
  - a periodic `/sync` every few minutes catches anything a dropped event missed.
- **First sync** (`since=0`, or after `410 sync.reset` → wipe first) is **state only** (#71):
  channels, members and users. Messages come by paging:
  - opening a channel pages `before=` from the newest cached message, or from the top;
  - `/sync` then keeps it current.
- **A channel new to the cache** (added to, re-added, or a DM someone opened): its full member
  list comes in the same page (#71). Its history is paged on open, as above.
- **`removed_channels`:** the channel, its messages, memberships, outbox entries and cached
  files are deleted in one transaction (the files after the commit).
- **Tombstones:** a deleted message keeps its row with an empty body and no attachments; its
  cached files are deleted.

## 5. Outbox (#63)
- A send creates the outbox row first (`client_id`: a UUIDv4 made at compose time), shown at
  once as **pending**. Then, when connected, core POSTs it with that `client_id`.
- **200 or 201:** the stored message replaces the pending one (matched by the echoed
  `client_id`), and the outbox row is deleted in the same transaction.
- **Order:** one sender per channel, oldest first; a failed row blocks later rows of its channel
  only (so a conversation never reorders).
- **Retries:** network errors retry with backoff and never mark the row failed. A 4xx refusal
  (403 removed or archived, 422 attachments) marks it **failed**, with the server's code and
  Retry and Delete actions. A 409 `conflict` (a `client_id` reused elsewhere) is a bug and is
  marked failed.
- **Attachments in an outbox entry:** each file is created with its own `client_id` (#73), PUT
  from the local copy, then attached. A PUT that loses to its own earlier attempt gets `409
  file.already_committed`; the stored `sha256` is compared with the local bytes, and equal
  means done.

## 6. File cache (#65, #67)
- **Downloads:** `GET /files/{id}/content` streamed to a temporary file inside the cache
  directory, resumed with `Range` + `If-Range: <sha256 ETag>` after an interruption, and checked
  against `FileOut.sha256` before it is moved into place. Files are stored encrypted
  (AES-256-GCM in 1 MiB chunks, key derived like the store's with `info = "brook.files.v1"`),
  because SQLCipher covers only the database.
- **Uploads:** streamed from the user's file with the upload's own per-request timeout (the
  client-wide 30 s doesn't apply to transfers); a 401 before the PUT starts refreshes, then
  re-opens the file.
- **Open / Save:** the app gets a decrypted copy only when the user opens or saves:
  - **Save** writes to the location the user chose;
  - **Open** writes a temporary copy in the app's temp directory, deleted when the app quits or
    on the next launch;
  - core never writes plaintext anywhere else.
- **Pinned** (`Keep available offline`): downloaded right away and never evicted. **Unpinned**:
  an LRU cap (default 1 GB per store) evicts least-recently-opened files.

## 7. Wipes
| Trigger | Effect |
|---|---|
| `410 sync.reset` | the store for that (server, user) is deleted and rebuilt from a state-only sync |
| Format version differs | the same |
| The device key is **missing** (`KeySlot` says absent) | every store is deleted, a new key is made |
| The key is **unreadable** (locked device, entitlement fault) | nothing is deleted; the app runs online-only until it can read the key (#46 §3.4) |
| A different user signs in on this server | that (server, user) is a different store; the other store is kept unless the user chose to remove it at sign-out |
| Sign-out with "Remove this device's data" ticked | that store and its files are deleted, after the logout call |
| A channel is removed | §4 |

## 8. Core API (FFI-exposed)
- `cached_channels()`, `cached_messages(channel, before?, limit)`, `pending_messages(channel)`.
- A `CacheEvent` stream: which channel changed. The UI re-reads; it doesn't get rows pushed.
- `send_message(channel, body, attachments)` → `client_id`: always through the outbox.
- `retry_send(client_id)` and `delete_pending(client_id)`.
- `open_file(file_id)` → a temp path; `save_file(file_id, destination)`.
- `pin_file` and `unpin_file`; `cache_state()` (online or offline, syncing, last synced).
- `logout(remove_data: bool)`, which extends today's `logout()`.

## 9. Tests (each seen failing under a named mutation)
- **Store:**
  - the file is unreadable without its key (no SQLite header, no plaintext strings);
  - the wrong key fails to open;
  - a format change deletes and rebuilds.
- **`seq` guard:**
  - a stale page after a live event keeps the live row;
  - replaying a page is idempotent;
  - a crash between rows and cursor (simulated) re-applies.
- **Sync against the TestServer:**
  - offline start serves the cache;
  - reconnect pages to `more=false`;
  - `410` wipes and rebuilds;
  - `removed_channels` deletes messages, outbox and files;
  - a new channel brings its members;
  - opening a channel pages history.
- **Outbox:**
  - pending → sent, matched by `client_id`;
  - a lost response and a resend make one message;
  - order within a channel;
  - a refusal marks failed and blocks only that channel;
  - an attachment whose PUT loses to itself is resolved by `sha256`.
- **Files:**
  - a resumed download with `If-Range`;
  - a sha256 mismatch is discarded;
  - encrypted at rest (no plaintext bytes in the cache directory);
  - the temp copy is deleted;
  - LRU eviction never removes a pinned file.
- **Wipes:** each row of §7, including "unreadable never deletes".
- **Live (itest):** go offline (block the server), read the cache, queue a message, come back,
  and the message is sent exactly once; an edit made elsewhere while offline shows up after
  reconnect.

## 10. Where this fails
| Failure | Response |
|---|---|
| SQLCipher adds libcrypto to the Linux build | Flatpak's runtime ships it; the tarball's INSTALL lists it (Linux client's call) |
| A very long offline period | `/sync` pages; if the server pruned tombstones past the cursor, `410` rebuilds |
| The same account on two devices edits while both are offline | the server decides by commit order; each cache follows `seq` |
| Disk full | writes fail, the app stays online-only and says so; the store is never half-written (SQLite transactions) |
