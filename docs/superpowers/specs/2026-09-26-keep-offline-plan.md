# Keep available offline: implementation plan (#67)

> Spec: 2026-09-26-keep-offline-spec.md. Two core PRs, then GTK. Each core PR is reviewed by
> the core owner and the server side. Every cached row has a lifecycle from the first PR on,
> so lifecycle and eviction land with the first cached byte, not later.

## Core PR 1: the cache, downloads, Open and Save

### 1. Schema (store.rs)
- `Kind::Cache` format 2 → 3 (pre-1.0: a mismatch rebuilds the cache, `open_reserved`
  already does this). Rename `CACHE_V1` to `CACHE_SCHEMA` while here.
- Replace `files` and add `message_files`:
  ```sql
  CREATE TABLE files(file_id TEXT PRIMARY KEY, sha256 TEXT NOT NULL, size INTEGER NOT NULL,
                     key BLOB NOT NULL, chunk INTEGER NOT NULL, state TEXT NOT NULL,
                     done INTEGER NOT NULL DEFAULT 0, pinned INTEGER NOT NULL DEFAULT 0,
                     last_used TEXT);
  CREATE TABLE message_files(file_id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                             channel_id TEXT NOT NULL);
  CREATE INDEX message_files_by_message ON message_files(message_id);
  CREATE INDEX message_files_by_channel ON message_files(channel_id);
  ```
- `remove_all` removes only the db files. A rebuilt cache leaves an old `files/` directory
  behind, and the reconciliation below removes it (every blob without a row goes).

### 2. The index and lifecycle, in apply's transaction (apply.rs, cache.rs)
One helper module, `file_rows.rs`, with functions taking `&Transaction`:
- `index_message(tx, message_id, channel_id, &[file_id])`:
  - replaces the message's `message_files` rows with its current list;
  - calls `drop_files` for every file that was listed before and isn't now (a list only
    shrinks).
- `drop_message(tx, message_id)`, `drop_channel(tx, channel_id)` and `drop_all(tx)`:
  - remove the `message_files` rows;
  - call `drop_files` for those ids.
- `drop_files(tx, ids)`:
  - deletes their `files` rows;
  - `INSERT OR IGNORE INTO deletions(path)` for each blob (`files/<file_id>`);
  - returns the ids, for the `Files` event.

Call sites (the explore map):
- the upsert in `apply` (apply.rs ~182-225, inside `if changed > 0`) calls `index_message`
  with the stored JSON's `attachments`;
- the tombstone branches (apply.rs ~226-253) call `drop_message`;
- `remove_channel_rows` (apply.rs ~377) calls `drop_channel`, and its doc comment already
  names this;
- `Cache::reset_rows` (cache.rs ~247) calls `drop_all` in its transaction, which is #67's
  reset note;
- `Applied` gains `dropped_files: Vec<String>`, and the callers that commit emit
  `CacheEvent::Files(ids)` after the commit.

**Unlinking** happens after commit, off the transaction: `Files::sweep_journal()` removes
the journalled paths (NotFound counts as done) and deletes their journal rows. It's called
after each apply that dropped files, and at open.

### 3. The streaming sealer (snapshot.rs)
- Pull the per-chunk step out of `write` into `pub(crate) struct Sealer`:
  - `new(key, id, layout)`, knowing the total size, so the last flag comes from
    `Layout::chunks()` and not from reading ahead;
  - `push(&mut self, bytes) -> Vec<sealed chunks>`, which seals every full chunk;
  - `finish()`, which seals the last one;
  - `at(index)` for a resume, from an empty buffer at a chunk boundary.
- `write` becomes a loop over `Sealer`: same bytes, same tests.
- Make `Layout`, `open_chunk`, `aad` and `nonce` `pub(crate)`. `id_bytes` takes any
  canonical UUID, and server file ids are UUIDs.
- `Decrypting` gets a start index for Save-from-cache and Open. Both read from 0 today, so
  this is only a constructor argument for now.

### 4. `EncryptingSink` (files.rs), a `DownloadSink`
- **`resume_offset`** is `done` from the row. The blob is truncated to
  `layout.sealed_offset(done / chunk)` before appending. A blob shorter than recorded means
  `restart`.
- **`write_chunk`** pushes into the sealer and appends sealed chunks. Every 8 chunks it runs
  `fsync(blob)` and then `UPDATE files SET done`, never the other order, so `done` never
  points past durable bytes.
- **`restart`**: a new random key, truncate to 0, `done = 0` and the row's key replaced, in
  one db call, before any byte is written under the new key.
- **`finish`**:
  1. seal the last chunk and fsync the file and its directory;
  2. `snapshot::verify` against the row's sha256 and size;
  3. `state = complete`.

  A verify failure means `restart` and `SinkError::Mismatch`.
- **`abort`**: fsync what's there and record `done`. The partial stays for a later resume.
- Blocking work runs in `spawn_blocking`, and the sink holds a small async-friendly buffer.

**The resume invariant (spec §4.1).** `download_inner` already continues only on a `206`
whose `ETag` equals `"<sha256>"` and whose `Content-Range` starts at the offset. Any `200`
calls `restart`. The plan adds tests that pin this for the encrypting sink: a `206` with
the wrong start, and a `200` after a partial, both give a new key.

### 5. The `Downloader` (transfer.rs)
- Move `download_inner` and `stream_into` into
  `pub(crate) struct Downloader<'a> { http, base, transfers, token: &dyn TokenSource }`,
  mirroring the `Uploader`.
- `download_file` uses it with `CurrentToken`, so its behaviour doesn't change.
- The loop also checks `flags.stopped()`, so a pause (`transfer.paused`) stops a download
  just as a cancel does.
- **No content coding, ever** (production finding: a proxy gzipped download bodies, which
  rewrote the ETag to `"<sha256>-gzip"` and sent a gzipped body under an identity
  `Content-Range`):
  - the transfer client never enables `reqwest`'s `gzip`, `brotli`, `deflate` or `zstd`,
    and sends `Accept-Encoding: identity` explicitly on downloads;
  - a test asserts the core crate's `reqwest` features exclude them;
  - any `Content-Encoding` other than `identity` on a download response is a failed attempt
    that can't be resumed: the sink `restart`s (a new key), and the attempt counts toward
    `MAX_ATTEMPTS` as a transient error.
- A `404` becomes `Error::Api { code: "file.gone" }` (was: `api(404)`). `download_file`
  keeps returning it, and `Save` shows it as today's "not found" text in the apps.

### 6. `Files` (files.rs), per signed-in user, next to the outbox
- `Files::open(cache_db, &store_dir, session_rx, transfers, net)`, called in
  `Offline::signed_in` right after `Cache::new` (offline.rs ~272). It runs the
  best-effort **reconciliation**:
  1. create `<store>/files` (0700 via the store dir);
  2. sweep the `deletions` journal;
  3. delete every blob with no `files` row;
  4. set `partial` rows whose blob is missing or shorter than `done` back to `done = 0`
     under a new key;
  5. empty the Open directory for this store.
- Its token is the `EpochToken` from cache_http.rs, bound to the store's epoch. A watcher
  sets `pause` on every in-flight download when the session leaves the epoch, as
  `Outbox::upload_files` does.
- **Single flight:** `inflight: Mutex<HashMap<file_id, Shared>>`.
  - `Shared` holds the download's `Arc<Flags>`, a `watch` of its result, and the set of
    joined caller `TransferId`s. Progress is re-emitted under each joined id.
  - A caller's `cancel_transfer` removes its id. When the set is empty and the file isn't
    pinned, the shared flags are cancelled.
  - The `Transfers` registry learns about joined ids through `register`/`unregister`
    (already there for outbox rows).
- **`cache_file(id, file_id)`**:
  1. look up `message_files`, and get `FileInfo` from that message's JSON (`file.unknown`
     if absent);
  2. `complete`: touch `last_used` and return;
  3. otherwise make or keep the `files` row and join or start the download with an
     `EncryptingSink`;
  4. on `file.gone`: `drop_files` for that id (row, journal, event).

  Afterwards eviction runs (§8).
- **`open_file(id, file_id)`**:
  1. `cache_file`;
  2. sniff the first 64 plaintext bytes (`open_refused` if they are ELF, `MZ`, a Mach-O
     magic, `#!`, or start with `[Desktop Entry]` after optional BOM and whitespace);
  3. decrypt into `<open dir>/<random 16 hex>/<FileInfo.filename>` with mode 0600 in a
     0700 dir; on the Mac, set the `com.apple.quarantine` xattr;
  4. touch `last_used`, then return the path.
- **`save_file(id, file_id, dest)`**:
  - `complete` in the cache: decrypt into a `FileSink` on `dest`, check the sha256 while
    decrypting, and `abort` (remove) on mismatch;
  - else the `Downloader` straight into `dest` (today's path, via `download_file`
    semantics).
- **`clear_open_copies()`**: removes this store's Open directory contents. It's public on
  `BrookClient`, and also run at `enable_local_data` and on sign-out with data removal.
- **The Open directory:**
  - `$XDG_RUNTIME_DIR/brook/<store_id>/` on Linux;
  - `$XDG_RUNTIME_DIR/app/$FLATPAK_ID/brook/<store_id>/` under Flatpak (from
    `/.flatpak-info`'s `name=`);
  - `std::env::temp_dir()/brook/<store_id>/` on macOS;
  - if `XDG_RUNTIME_DIR` is unset, the Open action is refused with `local.unavailable`
    (no fallback to /tmp).

### 7. Public API (client_offline.rs)
- `cache_file`, `open_file`, `save_file` and `clear_open_copies`, all
  `local.unavailable` without local data.
- `CacheEvent::Files(Vec<String>)`.
- `file_state(file_id) -> FileCacheState` (spec §4.5). Pins arrive in PR 2; PR 1 returns
  only `NotCached`, `Partial` or `Cached`.

### 8. Eviction (files.rs)
- `FILE_CACHE_CAP: u64 = 1 << 30`.
- After a completed download: if `SUM(size)` over rows with `pinned = 0` exceeds the cap,
  drop rows in this order, via `drop_files` (journal, then sweep), until under the cap:
  1. `state = 'partial'` by `last_used`;
  2. then `complete` by `last_used`.

  The file just finished is excluded.

### 9. Tests (PR 1)
- **Sealer:** chunk-for-chunk equality with today's `write`; a resume at chunk k gives the
  same ciphertext as one pass under the same key; the last flag comes from the size.
- **Sink** (with the existing test HTTP server):
  - a crash between the fsync and the `done` update resumes from the older `done`, and
    verify passes;
  - a `200` after a partial means a new key and verify passes;
  - a `206` at the wrong offset means a new key;
  - a truncated blob means a restart;
  - a sha mismatch at finish means a restart and a `Mismatch` error.
- **Lifecycle, each in apply's transaction:**
  - a tombstone drops the file;
  - a `message.update` with a shorter list drops only the missing file;
  - channel removal and `reset_rows` drop everything;
  - the journal is swept after commit;
  - a `404` gives `file.gone` and drops the file.
- **Single flight:** a second `cache_file` joins; cancelling one caller keeps the other;
  cancelling both stops it and keeps the partial.
- **Content coding:** a `206` or `200` carrying `Content-Encoding: gzip` restarts under a new
  key and never appends those bytes; the request carries `Accept-Encoding: identity`.
- **Tokens:** a session change mid-download pauses it and never sends another epoch's
  token (the test server checks the bearer).
- **Open:** refuses ELF, `MZ`, `#!` and `.desktop`, and allows a PDF; the copy lands in the
  per-store dir with the right modes; `clear_open_copies` and reconciliation empty it.
- **Save:** from cache while the server is down.
- **Eviction:** the order and the cap, and the file just finished stays.
- **Reconciliation:** orphan blobs, the journal, a short partial, and a leftover `files/`
  after a format rebuild.

## Core PR 2: pins
- `pin_file(file_id)` / `unpin_file(file_id)`: `pinned` on the row, making it (`partial`,
  `done = 0`) if absent; `file.unknown` rules as in PR 1.
- **The fetcher**, one task per `Files`:
  - wakes at open, on `pin_file`, and when `CacheState.offline` goes from true to false;
  - fetches each incomplete pinned file through `cache_file` under a core-made
    `TransferId`, one at a time;
  - backs off on transient errors;
  - on `file.gone`, the lifecycle has already dropped the pin.
- `file_state` reports `Pinned { cached, transfer }` with the fetcher's id.
  `pinned_bytes()` is `SUM(size) WHERE pinned = 1`.
- Eviction already skips pins. Unpinning makes a file an ordinary LRU entry.
- **Tests:**
  - the fetcher resumes after restart and when back online;
  - Open during a pin fetch joins it, and cancelling the Open keeps the pin's download;
  - an unpinned file is evictable;
  - a pin on a file that goes is dropped with it.

## FFI (the core owner's)
Same functions over the Apple bindings, with `open_file` returning a path string. It lands
after core PR 1 and PR 2, and is the core owner's call.

## GTK
- **After core PR 1:**
  - the attachment row's main button becomes **Open**, with Save… in a menu;
  - progress and Cancel work as today;
  - `open_refused` shows "Save it instead", `file.gone` shows "No longer available", and
    when offline and not cached the buttons are insensitive with the tooltip;
  - `clear_open_copies()` runs on quit;
  - rows follow `CacheEvent::Files` through `file_state`.
- **After core PR 2:** the **Keep available offline** check item, plus the "Available
  offline" and "Downloading for offline" states.
- **Live check** against the test server: pin, go offline (the proxy switch from the
  earlier smoke run), Open; delete the file on the server, and the row shows "No longer
  available" and the blob is gone.
