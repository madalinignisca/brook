# Outbox: attachments (#65 send side, #63)

Status: spec, closed after review round 2. Builds on the offline-cache design (2026-09-25-offline-cache-design.md §5.4,
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
     so a chooser can grey a file out. They are the server's **defaults** (`files_max_bytes`
     is operator-configurable): a server set lower refuses at upload (413, handled below);
     one set higher is capped by the client until a limits endpoint exists. The server's 5 GiB per-user quota (`413 file.quota_exceeded`) can't be
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
   deleting the source file afterwards changes nothing. Snapshots are written first and
   **fsynced (file and directory) before the row commits**, so a committed row never points
   at a snapshot a power cut could lose. A crash before the commit leaves orphan snapshots,
   which reconciliation removes (6). The stored `size` is the bytes actually copied, not a
   size read before the copy (a file growing mid-copy can't give the PUT a wrong length).
   **The same `client_id` again** returns the stored row's receipt (its files, with the
   `transfer_id`s this process already issued for them; one per file per process). The `client_id` is looked up **before** anything is copied; only a
   repeat racing an uncommitted first call copies, and it removes its snapshots before
   returning. The stored row wins, files included, as for body and reply target. A
   repeat after the row was acknowledged (it's gone) queues and uploads again. The POST
   then returns the stored message, and the new uploads are server-side orphans until the
   24 h sweep. That's rare and bounded, and stated rather than tracked.
3. **Snapshot format** (design §6.1): a random 256-bit key per snapshot, kept in its
   `outbox_files` row inside the encrypted outbox. 1 MiB chunks sealed with AES-256-GCM.
   The nonce is the chunk index (96-bit big-endian), and AAD is fixed-width: the 16 bytes of
   `file_client_id` (a UUID), the chunk index as u64 big-endian, and one byte for the
   last-chunk flag. A snapshot is written once, under a key made for it; a rewrite (a
   repeat call, a re-snapshot) gets a new key, so a nonce is never reused under one key.
   The plaintext sha256 is computed while copying and stored.
   **A snapshot is verified before its first byte is uploaded** (decrypt every chunk,
   check the flagged last chunk and the sha256; a read, no plaintext written), once per
   file per process: the mark is kept in memory, so a retry doesn't re-read 1 GiB each
   minute. It is verified again only after a read error during a PUT. A failed
   verification fails the row with `outbox.snapshot_damaged`: nothing of it is uploaded,
   and it isn't retried. A read error during the PUT itself (after a good verification) is
   a transient local fault.
4. **Sender, per row with files, in order.** Every upload request (create, PUT, and each
   retry of either) runs **under the row's session epoch**, like the message POST. The
   token and the epoch are read **together, from one session snapshot** (as
   `cache_http`'s `Post::send` does), and the token is used only if the epoch is the
   sender's; otherwise the attempt stops and the row waits. A user switch can't upload one
   user's snapshot with another's token. `upload_file`'s internals take that token rather
   than calling `access_token()` themselves. The epoch is checked again between files.
   **Signing out pauses** the outbox's in-flight transfers: a stop separate from cancel,
   which leaves the row `pending` with no cancel flag and reports `Retrying`. They resume,
   by `file_client_id`, at the next sign-in of that user.
   A pending upload the server swept mid-transfer (1 h after its creation, e.g. on a very
   slow link: the PUT answers `file.upload_expired` or 404) is created again by its
   `file_client_id`, which gives a fresh pending file, and its bytes are PUT again.
   A row with files holds its channel's queue while it uploads (per-channel order is the
   outbox's rule): a later text message in that channel waits for it. That is stated to
   the user by the pending row's progress, and other channels are unaffected.
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
   - transient (network, 5xx including `507 file.no_space`, 408/409 in progress, 429, and
     **401 / `NotAuthenticated`**, which waits for the session's refresh or the next
     sign-in, as a text row does): retried with backoff; the row stays pending, and the
     file's transfer events say `Retrying`, never `Failed`;
   - an upload refused for good (other 4xx) fails the row with the server's code, and that
     file's `PendingFile.error` carries it, so the UI can say which file was refused;
   - `422 file.not_attachable` on the POST re-uploads once (3a), then fails the row;
   - `413 file.too_large` / `file.quota_exceeded` on an upload fail the row;
   - **cancel** is for the row, through any of its files' `transfer_id`s. It sets the
     row's cancel flag, checked before each create, between PUT chunks, and before the
     POST. A file not started yet never starts; an uploading file stops at the next chunk;
     an uploaded file stays uploaded (its id is kept for Retry). The row then fails with
     `transfer.cancelled`. A cancel that arrives after the POST was sent is too late: the
     message may already exist, and the ack decides. **Retry clears the flag.**
   Retry and Delete work as for any failed row. **Retry** re-uses the uploaded `file_id`s
   and re-uploads only what's missing. **Delete** removes the row, its `outbox_files` rows
   and (after the commit) its snapshots. **Delete of an uploading row cancels its
   transfers first** (the cancel takes no lock the sender holds), then waits for the
   channel's lock, so a Delete tap never waits for hours of upload. A row's cancel flags
   are removed with the row.
   `PendingMessage` gains `files: Vec<PendingFile { file_client_id, transfer_id, filename,
   size, uploaded, error }>`, so the UI can draw per-file progress and state after a
   restart. Transfer ids are re-issued at startup, and `pending_messages` returns the
   current ones.
6. **Reconciliation** (design §7.3) runs for the outbox store after it opens with its key
   and its tables were read, and **finishes before the outbox takes an enqueue** (inside
   `Outbox::open`), so a snapshot being written for an uncommitted row can never look
   like an orphan. It finishes journalled deletions and removes snapshot files with no
   `outbox_files` row. With an unreadable key, nothing is removed.
7. **Direct send** (outbox not writable): a message with files is refused with
   `outbox.store`. Files can't be sent without a snapshot, and a direct upload from the
   source file would break "the bytes you queued are the bytes sent". With **no** outbox
   (its key locked or damaged), files can't be sent at all; that's the design's §5.7
   rule, unchanged here.
7a. **A message with files and no text:** the server requires a non-empty body today
   (`schemas.py`). Pending its answer, `send_queued_with_files` refuses an empty body with
   `outbox.empty_body` before anything is copied; if the server lifts the rule, this
   check goes.
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
    - duplicate ids never reach the POST;
    - an upload started under user A never continues under user B's token, and signing out
      cancels in-flight transfers;
    - a 401 on an upload leaves the row pending;
    - a damaged snapshot fails the row before any PUT;
    - cancel in each file state, and Retry after it;
    - Delete of an uploading row returns without waiting for the upload;
    - the same `client_id` again returns the stored receipt and leaves no extra snapshot;
    - reconciliation never removes a snapshot of a row being enqueued;
    - a row uploading at sign-out is `pending` afterwards and resumes at the next sign-in;
    - a pending upload swept mid-transfer is re-created and completes.

## Review round 1 (adversarial review of the spec)

Taken: epoch-bound uploads and cancel on sign-out (4); 401 and 507 transient (5);
verification before upload (3); fsync before commit, size from the copy, a key per write,
fixed-width AAD (2, 3); reconciliation before enqueue (6); a repeat call returns the stored
receipt (2); an empty body refused until the server says otherwise (7a); cancel per file
state and Delete cancelling first (5); a per-file error (5); `Retrying` rather than
`Failed` events for retried uploads (5); head-of-line blocking stated (4); the 24 h sweep
(4.3a, added from the server's facts while the review ran).

## Review round 2

Taken: sign-out pauses rather than cancels (4); verification once per file per process
(3); the repeat lookup before copying, and a repeat after the ack stated (2); a swept
pending upload re-created (4); token and epoch from one snapshot (4); limits as server
defaults (1). Attachment order across later reads depends on the server storing the
requested position; the server side has taken that fix, and a /sync-order test is added
here when it lands.

## Not doing

- The Mac and GTK composer UIs (#66): file chooser, chips, remove-before-send (a composer
  concern: nothing is queued until Send), per-file progress bars.
- Downloads into the cache, pinning, eviction and Open copies (#67, design §6.2, §6.4).
- Image thumbnails or previews.
- A local cap on snapshot storage (10 × 100 MiB per queued message is bounded; the server's
  5 GiB per-user quota only bites at upload).
