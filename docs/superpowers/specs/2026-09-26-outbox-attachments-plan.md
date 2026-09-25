# Outbox attachments: plan

Spec: `2026-09-26-outbox-attachments-spec.md` (closed after round 2). Dial: Heavy. One PR, in
commits that each build and pass on their own.

## Steps

1. **`snapshot.rs` (new): the encrypted snapshot, alone and fully tested.**
   - `write(src: &Path, dst: &Path, file_client_id: Uuid, progress: impl Fn(u64, u64))
     -> io::Result<Written { key: [u8; 32], size, sha256 }>`. It streams `src` in 1 MiB
     chunks, sealing each with `ring::aead::AES_256_GCM` under a fresh random key; the nonce
     is the index (u64 → 96-bit BE) and the AAD is the 25-byte fixed layout. The size comes
     from the bytes read, and the sha256 is of the plaintext. It fsyncs the file, then its
     directory. `src` is opened once.
   - `verify(path, key, file_client_id, size, sha256) -> Result<(), Damaged>` decrypts every
     chunk and checks the order, the flagged last chunk, the total and the sha256.
   - `SnapshotSource { path, key, id, size, sha256 }: transfer::UploadSource`. Its reader
     decrypts chunk by chunk (a `tokio::io::AsyncRead` over a buffered state machine), and
     `sha256()` returns the stored one.
   - Tests: round-trip; a flipped byte, swapped chunks, truncation, a dropped last chunk,
     another file's snapshot under this id; a source changed after the write; progress.
     This commit needs no other change.
2. **`transfer.rs`: upload with a caller's token, pausable.**
   - `create_upload` / `upload_inner` become free functions over `Uploader { http, base,
     transfers: Arc<Transfers> }` and a `&dyn TokenSource` (`async fn token(&self) ->
     Result<String>`).
   - `BrookClient::upload_file` passes one that reads `access_token()`; it behaves as
     before.
   - The outbox passes an `EpochToken { session, epoch }`, which reads token and epoch from
     one `snapshot()` and answers `NotAuthenticated` on a mismatch.
   - `TransferState::Preparing` is added. `Transfers` gains a pause flag per id next to the
     cancel flag: a pause stops like a cancel, but surfaces as `transfer.paused` and emits
     `Retrying`, never `Cancelled` or `Failed`.
   - The existing transfer tests pass unchanged; new tests cover the epoch mismatch and the
     pause.
3. **`store.rs`: outbox format 3.**
   - `outbox_files(client_id, ordinal, file_client_id UNIQUE, filename, content_type, size,
     sha256, key, file_id NULL, error NULL)`, with ordinal the file's position.
   - `deletions(path PRIMARY KEY)`.
   - A format-2 outbox counts its unsent rows as in #119.
4. **`outbox.rs`: queued files.**
   - `Outgoing` gains `attachments: Vec<String>`, which `cache_http::send_body` puts in the
     JSON when it's non-empty.
   - `enqueue_with_files` does, in order:
     1. validates the limits and the empty body before anything else;
     2. looks up the `client_id`, and a stored row returns its receipt;
     3. writes the snapshots to `<store>/snap/<file_client_id>` (Preparing progress);
     4. commits the row and its files in one transaction;
     5. on a conflict (a racing first call), removes its own snapshots and returns the
        stored receipt.
   - A new network trait `Upload` sits beside `Post`:
     `upload(transfer_id, channel, file row, source, epoch) -> Result<FileInfo,
     SendFailure>`. `cache_http::Http` implements it through step 2's `Uploader` with
     `EpochToken`, and it maps errors with `transfer::is_transient` plus 401 and 507 as
     transient.
   - `attempt` for a row with files:
     1. for each file without a `file_id`, in ordinal: check the epoch, verify once per
        process, upload, then commit its `file_id`. A permanent error stores it as the
        file's `error` and fails the row;
     2. check that the ids are distinct;
     3. POST with the ids in order. A first `file.not_attachable` clears all the `file_id`s
        and runs step 1 again in the same attempt; a second one fails the row.
   - The ack: rows out, then journal and unlink the snapshots, then clear the journal.
   - Cancel: a per-row flag, set through any of its transfer ids (a map from the row's
     current transfer ids to the row), checked before each create, between chunks (the
     transfer's cancel flag) and before the POST.
   - Delete: set the row's cancel and its transfers' cancel **before** taking the channel
     lock.
   - Sign-out (the session watch goes `None`): pause every live outbox transfer.
   - `Outbox::open`: finish the journal, then remove any `snap/*` without an
     `outbox_files` row, all before `open` returns.
   - `pending()` fills `files` with the current transfer ids (issued lazily per process).
5. **`client_offline.rs` and FFI:**
   - core: `send_queued_with_files`, `OutgoingFile`, `SendReceipt`, `QueuedFile`,
     `PendingFile`, and the consts;
   - FFI: the records, a `send_queued_with_files`, and `TransferState::Preparing` in the
     FFI's transfer events (check whether they're exported yet; if not, export them
     here);
   - the Swift package is rebuilt and the Swift tests run.

## Where this fails

- **Crypto misuse.** A key reused across writes, or a nonce reused: every write makes a
  new key, and a test proves two writes of the same file differ.
- **An upload under the wrong user:** `EpochToken`, with a test of a user switch
  mid-upload.
- **Deadlock:** Delete's cancel touches only atomics and the id map, never
  `sender.lock`. The sender checks the flags without holding the map lock across an
  await.
- **A slow test suite:** tests use 3 small files with a 1 MiB chunk size configurable
  down (a `#[cfg(test)]` const); only one test crosses the chunk boundary with real
  sizes.
- **The FFI's transfer events** may not be exported yet: the Mac UI needs them for
  progress, so step 5 checks and adds them.

## If it stops halfway

- After 1: an unused module with tests. Safe.
- After 2: uploads behave as before, plus a pause state. Safe.
- After 3 without 4: the format is bumped with nothing using the new tables, and beta
  outboxes are rebuilt for nothing. Don't merge 3 without 4.
- After 4 without 5: core is complete, but no app API. Safe to merge; the FFI follows.
