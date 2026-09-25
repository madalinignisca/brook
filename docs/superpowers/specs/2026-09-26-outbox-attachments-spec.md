# Outbox: attachments (#65 send side, #63)

Status: spec. Builds on the offline-cache design (2026-09-25-offline-cache-design.md §5.4,
§6.1, §6.3, §7.3) and `transfer.rs`. Review dial: **Heavy**. It encrypts user files at rest,
deletes local data (snapshots), and changes the outbox format.

## Done means

1. **API (core, then FFI):**
   `send_queued_with_files(channel, body, reply_to_id, client_id, files: Vec<OutgoingFile>)
   -> SendReceipt`.
   - `OutgoingFile { path, filename, content_type }`: `filename` is the display name as the
     user sees it; `content_type` is declared and untrusted.
   - `SendReceipt { client_id, files: Vec<QueuedFile { file_client_id, transfer_id, size }> }`.
   - `send_queued` stays as it is, for text only.
   - Checked **before anything is copied** (a 2 GB video is refused at once, not after an
     encrypted copy the server would refuse): at most `MAX_FILES_PER_MESSAGE` = 10
     (`outbox.too_many_files`), each non-empty (`outbox.empty_file`) and at most
     `MAX_FILE_BYTES` = 100 MiB (`outbox.file_too_large`). Both limits are public consts,
     so a chooser can grey a file out, and they match the server's (`files_max_bytes`, the
     message limit). The server's 5 GiB per-user quota (`413 file.quota_exceeded`) can't be
     checked locally; it fails the row like any refusal.
   - **Each path is read only during this call** (a Flatpak portal path may be readable
     once): the snapshot is made from it before returning, and it is never opened again.
   - **Snapshot progress:** while a file is copied, its `transfer_id` reports
     `TransferState::Preparing` with bytes done and total on `transfer_events()`, so the
     composer can show real progress for big files. The call itself stays async and
     should be called off the UI thread.
2. **Queued means durable, snapshots included.** Before `send_queued_with_files` returns,
   each file has been copied into the user's store as an encrypted **snapshot**, and the
   row plus its `outbox_files` rows are committed in one transaction. Editing, moving or
   deleting the source file afterwards changes nothing. Snapshots are written first; a
   crash before the commit leaves orphan snapshots, which reconciliation removes (6).
3. **Snapshot format** (design §6.1): a random 256-bit key per snapshot, kept in its
   `outbox_files` row inside the encrypted outbox. 1 MiB chunks sealed with AES-256-GCM.
   The nonce is the chunk index (96-bit big-endian), and AAD = `file_client_id | index |
   last-chunk flag`. The plaintext sha256 is computed while copying and stored. A snapshot
   that fails authentication, is truncated, or lacks its flagged last chunk fails the row
   with `outbox.snapshot_damaged`; it is never uploaded as it stands.
4. **Sender, per row with files, in order:**
   1. each file without a `file_id` is uploaded through `transfer.rs`'s upload path, from a
      decrypting `UploadSource` over the snapshot, with its `file_client_id` and the
      receipt's `transfer_id`. Progress arrives on `transfer_events()`. `409
      file.already_committed` with an equal sha256 counts as done.
   2. each resulting `file_id` is committed to its `outbox_files` row as it arrives, so a
      restart resumes after the last one.
   3. then the message is POSTed with `attachments: [file_id…]` in the row's order, plus the
      body and reply target, as for any row. The server keeps request order (a server fix
      is under way so every later read does too). Ids are checked distinct before the POST
      (a duplicate is a bug: the row fails with `outbox.duplicate_file`, never sent).
   3a. **Files swept meanwhile.** The server deletes committed-but-unattached files 24 h
      after their creation, and pending ones after 1 h. So a first `422
      file.not_attachable` on the POST clears the row's stored `file_id`s and runs step 1
      again. Each file's create with its `file_client_id` returns the same committed file
      if it survived, or a fresh pending one (new id) whose bytes are PUT again from the
      snapshot. Then the POST is retried. A second `not_attachable` fails the row. The
      message's `client_id` is checked before its attachments, so a send the server had
      already accepted still returns the stored copy. This is why snapshots live until
      the acknowledgement.
   4. on the acknowledgement the row and its `outbox_files` rows go in one transaction, and
      the snapshot files are removed after that commit (journalled like cache deletions:
      the design's `deletions` table in outbox.db).
5. **Failures:**
   - transient (network, 5xx, 408/409 in progress, 429): retried with backoff; the row
     stays pending;
   - an upload refused for good (4xx) fails the row with the server's code;
   - `422 file.not_attachable` on the POST re-uploads once (3a), then fails the row;
   - `413 file.too_large` / `file.quota_exceeded` on an upload fail the row;
   - cancelling a queued file's `transfer_id` fails the row with `transfer.cancelled`.
   Retry and Delete work as for any failed row. **Retry** re-uses the uploaded `file_id`s
   and re-uploads only what's missing. **Delete** removes the row, its `outbox_files` rows
   and (after the commit) its snapshots.
   `PendingMessage` gains `files: Vec<PendingFile { file_client_id, transfer_id, filename,
   size, uploaded }>`, so the UI can draw per-file progress and state after a restart.
   Transfer ids are re-issued at startup, and `pending_messages` returns the current ones.
6. **Reconciliation** (design §7.3) runs for the outbox store after it opens with its key
   and its tables were read. It finishes journalled deletions and removes snapshot files
   with no `outbox_files` row. With an unreadable key, nothing is removed.
7. **Direct send** (outbox not writable): a message with files is refused with
   `outbox.store`. Files can't be sent without a snapshot, and a direct upload from the
   source file would break "the bytes you queued are the bytes sent".
8. **Wipes** (sign-out with "Remove this device's data", other users, a lost key) remove the
   snapshot directory with the store, as they already do for everything under it.
9. **Outbox format 3**: the `outbox_files` table as used here, the `deletions` journal, and
   `outbox_files.ordinal` for attachment order. A format-2 outbox with unsent rows is
   reported lost and rebuilt, as in #119.
10. Tests, each watched failing under a mutant:
    - the snapshot round-trips, and a flipped byte, a reordered chunk, a truncation or a
      missing last chunk each fail;
    - the source changed after enqueue: the snapshot's bytes are uploaded;
    - a lost upload answer resumes by `file_client_id`, with no second file;
    - a restart after two of three uploads uploads only the third;
    - the POST carries the ids in order, with the reply target;
    - ack, Delete and reconciliation leave no snapshot files; an unreadable key deletes
      none;
    - cancel fails the row, Retry resumes it;
    - 10 files are accepted and 11 refused, and a file over 100 MiB is refused, with nothing
      written;
    - `Preparing` progress arrives during the snapshot;
    - a first `not_attachable` re-creates the files (a swept one gets a new id and its bytes
      again), and a second fails the row;
    - duplicate ids never reach the POST.

## Not doing

- The Mac and GTK composer UIs (#66): file chooser, chips, remove-before-send (a composer
  concern: nothing is queued until Send), per-file progress bars.
- Downloads into the cache, pinning, eviction and Open copies (#67, design §6.2, §6.4).
- Image thumbnails or previews.
- A per-user storage quota for snapshots (the server has none yet, #88).
