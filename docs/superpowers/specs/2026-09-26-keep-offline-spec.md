# Keep available offline: the file cache in core, and the apps (#67)

> Builds on the offline-cache design (2026-09-25-offline-cache-design.md §6, §7), which fixed
> the file cache's format, Open and Save, and pinning. This spec makes it buildable: the
> public API, what a cached file's life looks like, and the GTK UI. It also takes over the
> part of #65 that was left: the encrypted, disposable file cache.

## 1. What the user gets

- **Open** on an attachment opens it in the system's app for its type. The first Open
  downloads it into the cache. After that it opens with no connection until the cache
  evicts it.
- **Keep available offline** on an attachment downloads it now, or as soon as there is a
  connection, and keeps it until unmarked: this is the "document on the train" case. Pinned
  files are never evicted.
- **Save** writes the file where the user chooses. It comes from the cache when the file is
  complete there (so it works offline), else from the network as today.
- A file whose message was deleted is removed from the cache, pinned or not, and shows as
  "No longer available".

Not in scope: inline previews, drag and drop (#66), an "Offline files" list view, a
user-set cache size (the cap is a constant, §5).

## 2. Server facts it relies on

- `GET /files/{id}/content` supports `Range` and `If-Range`, with `ETag: "<sha256>"`. A `200`
  instead of a `206` means start over.
- Deleting a message deletes its files' bytes at once. There's no deferred delete yet (#83),
  so a download can meet a `404` midway. **A `404` means the file is gone**, not "retry".
- Files have no `seq` of their own. They ride on their message: the message's `attachments`
  list, its tombstone (`deleted_at`) and its channel's removal are how core learns a file
  went.
- A message's `attachments` list can only shrink after send, never grow:
  - an edit touches only `body`;
  - `DELETE /files/{id}` (the uploader, or a channel owner) removes an attached file's row
    and bytes at once, and restamps its message;
  - the message then arrives by `/sync` and by a live `message.update` with the current
    list.
- `GET` on a removed or pending file is `404` (never `403`).

## 3. Storage

- Blobs live at `<store dir>/files/<file_id>`, in the snapshot format (snapshot.rs):
  - per-file random 256-bit key;
  - AES-256-GCM in 1 MiB chunks, nonce = chunk index;
  - AAD = file id (16 bytes) | index | last-chunk flag.

  One format for both. `snapshot::write` becomes a thin wrapper over a streaming
  **sealer** (bytes in, sealed chunks out, resumable at a chunk boundary), which the
  outbox's copy and the cache's download sink both use. The decrypting reader and `verify`
  are shared as they are. The cache uses the file id's 16 bytes as the AAD id. A rewrite
  never reuses a key: a restarted download gets a new key.
- The `files` table grows (a cache format change; pre-1.0, so the cache is rebuilt, not
  migrated):
  - `files(file_id PK, message_id, channel_id, sha256, size, key, chunk,`
    `state, done, pinned, last_used)`;
  - `state` is `partial` or `complete` (a file is `complete` once its last chunk is written
    and `verify` passed);
  - `done` is the plaintext bytes in complete chunks: the resume point.
- A blob is only ever deleted through the `deletions` journal. Its path is recorded in the
  transaction that drops the row, and unlinked after the commit.
- Blobs stay under the store directory, so a store wipe (which removes the directory) still
  removes every one of them.
- **Reconciliation at cache open**, before anything uses the files. It is best-effort, as
  the outbox's is (design §7.3):
  - finish journalled deletions;
  - delete blobs with no row;
  - drop `partial` rows whose blob is missing or shorter than `done`;
  - empty the Open directory (§4.4).

## 4. Core API (`BrookClient`, with local data on)

All of these take a `TransferId` from the caller, as `send_queued_with_files` does, so the
UI can follow progress (`transfer_events`) and cancel (`cancel_transfer`) from the start.

**Whose token.** A download into a user's cache runs only under that user's session:
- It takes its token through the `Uploader`'s `TokenSource` plumbing, bound to the store's
  session epoch, not "whatever session is current".
- If the epoch moves (sign-out, another user signs in), the download pauses
  (`transfer.paused`) and never fetches with another user's token.
- Otherwise another user's file could be read into this user's cache.
- The download loop moves into a `Downloader` next to the `Uploader`. `download_file`
  keeps its current behaviour through it.

### 4.1 `cache_file(id, &FileInfo) -> Result<()>`
- Downloads into the cache, resuming a partial.
- An `EncryptingSink` (a `DownloadSink`):
  - buffers up to one chunk and seals each full chunk as it arrives;
  - fsyncs and updates `done` every few chunks, so a crash resumes from the last recorded
    chunk and never trusts bytes that weren't recorded;
  - `resume_offset` = `done`;
  - `restart` means a new key, truncation and `done = 0`;
  - `finish` seals the last chunk, then runs `verify` against `FileInfo.sha256` and marks
    the row `complete`.
- On a cancel the partial stays, for a later resume.
- A `404` drops the row and blob and returns `file.gone`.
- Already complete: returns at once and updates `last_used`.

### 4.2 `open_file(id, &FileInfo) -> Result<PathBuf>`
- `cache_file` first, then decrypts into the Open directory (§4.4) under
  `FileInfo.filename` (the server's sanitised ASCII name), inside a fresh random
  subdirectory so two opens never clash. Returns that path; the app hands it to the system
  (`gio::AppInfo::launch_default_for_uri`, `NSWorkspace`).
- **Refused for executables and launchers**, sniffed from the first bytes, never the name:
  - ELF `\x7fELF`, PE `MZ`, Mach-O magics, `#!`;
  - a `[Desktop Entry]` file.

  The refusal is `file.open_refused`, and Save stays available.
- Updates `last_used`.

### 4.3 `save_file(id, &FileInfo, destination) -> Result<()>`
- Complete in the cache: decrypts straight into `destination` (a `FileSink`, so the Flatpak
  portal rules hold), then checks the sha256.
- Otherwise: today's network download into `destination`. It does not also cache the file:
  Save is not Open.

### 4.4 The Open directory
- Linux: `$XDG_RUNTIME_DIR/brook/<store_id>/` (tmpfs, 0700), or
  `$XDG_RUNTIME_DIR/app/<app id>/brook/<store_id>/` under Flatpak.
- Mac: the app's temporary directory.
- It is the one plaintext copy core makes on its own. It is emptied by `clear_open_copies()`
  (the app calls it on quit), at the next `enable_local_data`, and on sign-out with data
  removal.

### 4.5 Pinning
- `pin_file(&FileInfo)`:
  - makes or keeps the row with `pinned = 1`;
  - if it isn't complete, a background fetcher downloads it now, or when the cache's state
    says online again;
  - the pin is durable, so it survives restarts, and the fetcher runs at
    `enable_local_data` for every incomplete pin;
  - fetcher progress arrives under a core-made `TransferId`, which `file_state` reports.
- `unpin_file(file_id)`: `pinned = 0`. The blob stays and is then an ordinary LRU entry.
- `file_state(file_id) -> FileCacheState`, one of:
  - `NotCached`;
  - `Partial { done, size, transfer: Option<TransferId> }`;
  - `Cached`;
  - `Pinned { cached: bool, transfer: Option<TransferId> }`.
- A `CacheEvent::Files(Vec<file_id>)` whenever a file's state changes (downloaded, evicted,
  gone, pinned), so a UI refreshes its rows.

### 4.6 Errors
- `file.gone` (404): the file was deleted.
- `file.open_refused`: an executable or launcher.
- `local.unavailable`: no local data, so there's no cache. Open then falls back to Save; the
  UI says so.
- `transfer.cancelled`, and the transfer layer's transient codes, as today.

## 5. Eviction

- Cap: 1 GiB of unpinned blobs per store (a constant, `FILE_CACHE_CAP`). Pinned blobs don't
  count and are never evicted.
- After each completed download, unpinned blobs are evicted in this order until under the
  cap:
  1. partials, oldest `last_used` first;
  2. then complete files, oldest `last_used` first.

  The file just downloaded is never evicted.
- A single file larger than the cap is still cached (so Open works). It's the first to go
  next time.

## 6. Lifecycle: when a cached file goes

In the same transaction as the cache change that causes it, rows are dropped and blobs
journalled. Pinned files go too, and the UI shows them as gone.

| Cause | Detected by | Effect |
|---|---|---|
| Its message deleted | a tombstone applied (live, `/sync`, history) | its files dropped |
| The file deleted from its message (`DELETE /files/{id}` by its uploader or a channel owner; an edit never changes files) | the message arrives (`message.update`, `/sync`, history) with that file missing from `attachments`, applied through the seq guard | that file dropped |
| Its channel removed, or I left it | channel removal applied | the channel's files dropped |
| A `404` on download | `cache_file` / the fetcher | that file dropped (`file.gone`) |
| `410 sync.reset` | `Cache::reset_rows` | every file row dropped and its blob journalled in `deletions`, **in the same transaction** as the other resets (the old server's ids mean nothing) |
| Sign-out with "Remove this device's data" | the store wipe | everything, as today |

## 7. GTK

- The attachment row gains:
  - **Open** as its main button;
  - a menu with **Save…** and a **Keep available offline** check item.
- Sizes and progress work as today: the progress bar follows the Open, Save or pin
  download, and Cancel stops it.
- A small status reads:
  - "Available offline" when pinned and cached;
  - "Downloading for offline" while fetching;
  - "No longer available" once the file is gone (then only the name stays, and the buttons
    go).
- Offline with the file not in the cache: Open and Save are insensitive, with the tooltip
  "Not downloaded: available when you're back online". Pinning still works and fetches later.
- `file.open_refused`: "This file can't be opened from Brook. Save it instead." Save stays.
- On quit the app calls `clear_open_copies()`.
- A `message.update` redraws the message's attachment rows from its `attachments`, so a
  file deleted from a message disappears on screen too. Today only the body is updated.

## 8. Tests

**Core:**
- **Downloads:** encrypted resume across a crash (never past recorded `done`); a `200`
  instead of a `206` means a new key and a restart; a `404` gives `file.gone` and removes
  row and blob.
- **Open:** refuses ELF, `#!` and `.desktop` files, and allows a PDF.
- **Save:** from cache while offline.
- **Pins:** the fetcher resumes after restart and when back online.
- **Eviction:** order and the cap; a pinned file is never evicted.
- **Lifecycle:** every row of §6.
- **Reconciliation:** journal, orphans and the Open directory.

**GTK:** the state texts, and a live run against the test server: pin, go offline, Open.

## 9. Order of work

1. Core: schema and `EncryptingSink`, then `cache_file` / `open_file` / `save_file`, then
   pins and the fetcher, then eviction and lifecycle. Two PRs:
   - download and Open/Save;
   - pins, eviction and lifecycle.
2. FFI for the Apple app: the core owner's call.
3. GTK UI (after core PR 1 for Open and Save, after PR 2 for pinning).
