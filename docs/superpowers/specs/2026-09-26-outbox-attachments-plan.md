# Outbox attachments: plan

Spec: `2026-09-26-outbox-attachments-spec.md` (closed after round 2). Dial: Heavy. One PR, in
commits that each build and pass on their own. Revised after plan review round 1; that
section is at the end.

## Steps

1. **`snapshot.rs` (new): the encrypted snapshot, alone and fully tested.**
   - `write(src, dst, id: [u8; 16], chunk: usize, progress) -> io::Result<Written { key,
     size, sha256 }>`.
     - `id` is the `file_client_id`'s 16 bytes, hex-decoded from its canonical form (no
       uuid crate).
     - `chunk` is a parameter (production: `CHUNK` = 1 MiB). Tests use small chunks with
       the same format; one test uses the real size.
     - It streams `src` (opened once) and seals each chunk with AES-256-GCM under a fresh
       random key. The nonce is the index as a 96-bit big-endian number. The AAD is 25
       bytes: `id` (16), index as u64 BE (8), and the last-chunk flag (1).
     - `size` is the bytes read, and `sha256` is taken over the plaintext.
     - It fsyncs the file, then the directory. It never creates the directory: `snap/`
       exists only because `Outbox::open` made it, so a write racing a wipe fails rather
       than recreating a store directory.
   - `verify(path, key, id, size, sha256, chunk) -> Result<(), Damaged>` checks every chunk,
     the order, the flagged last chunk, the total and the sha256.
   - `SnapshotSource: transfer::UploadSource`: a decrypting `AsyncRead`, and `sha256()`
     returning the stored value.
   - The file work (`write`, `verify`) runs in `spawn_blocking`. It never runs on the
     runtime's two workers, where 1 GiB of crypto would stall the WebSocket. Progress
     crosses back through the broadcast sender. A caller that drops its future doesn't
     stop the copy; the copy completes and commits, and the spec allows that ("queued
     means durable").
   - Tests:
     - a round trip, at a small chunk size and at the real one;
     - two writes of the same file give different ciphertext (a new key);
     - a flipped byte, swapped chunks, truncation, a dropped last chunk, and another
       file's snapshot under this id all fail;
     - a source changed after the write doesn't change what is read;
     - progress events arrive.
2. **`transfer.rs`: uploads with a caller's token, cancel/pause per row, bounded creates.**
   - `BrookClient.transfers` becomes `Arc<Transfers>`: `new()` and every `self.transfers`
     user change.
   - `create_upload` / `upload_inner` become functions over `Uploader { http, base,
     transfers: Arc<Transfers> }` plus a `&dyn TokenSource`.
     - `BrookClient::upload_file` passes a source that reads `access_token()`, and keeps
       emitting its own final event as now.
     - The outbox passes `EpochToken { session, epoch }`, which reads token and epoch from
       one `snapshot()` per request and answers `NotAuthenticated` on a mismatch. One PUT
       is one request with one token, so a user switch can't mix tokens within one.
   - **Flags per row, synchronously reachable.** `Transfers` gets a std-mutex map from
     `TransferId` to `Arc<RowFlags { cancel, pause }>`.
     - The outbox registers a row's transfer ids in it **before** any transfer starts.
     - `cancel_transfer(id)` stays synchronous. It sets the row's cancel flag through the
       map, or only the id's own flag for an id outside the map (`upload_file`'s case, as
       today).
     - Uploads check both flags, cancel and pause, before each request and between
       chunks. `create_upload`'s own retries check them too, and sleep through
       `wait_or_cancel` (no plain `sleep`), so a Delete or a pause is never stuck behind a
       60 s `Retry-After`.
     - A pause ends as `transfer.paused`, a transient error, and emits `Retrying`.
   - **The final event is the caller's.** The upload core returns its result without
     emitting an end state. `upload_file` emits as before; the outbox emits `Retrying` for
     transient outcomes and `Failed` only when it fails the row.
   - Tests: the existing transfer tests unchanged; an epoch mismatch; a pause mid-PUT and
     during a create's backoff; a cancel through the row map.
3. **`store.rs`: outbox format 3.**
   - `outbox_files(client_id REFERENCES outbox(client_id) ON DELETE CASCADE, ordinal,
     file_client_id UNIQUE, filename, content_type, size, sha256, key, file_id NULL, error
     NULL)`. `foreign_keys` is already on.
   - `deletions(path PRIMARY KEY)`.
   - A format-2 outbox counts its unsent rows, as in #119.
4. **`outbox.rs`, `offline.rs`, `cache_http.rs`: queued files.**
   - **Plumbing.**
     - `Outbox::open` gains the store directory. It creates `snap/`, finishes the journal,
       and removes `snap/*` files without an `outbox_files` row, all before returning.
       This cleanup is best-effort: an unlink that fails is logged and skipped, and never
       stops the outbox opening.
     - `offline::signed_in` passes the directory from `open_user`.
     - `Net` gains `upload: Arc<dyn Upload>`, and `BrookClient::net()` gives `Http` the
       `Arc<Transfers>`.
   - `Outgoing` gains `attachments: Vec<String>`; `send_body` puts it in the JSON only when
     non-empty.
   - `enqueue_with_files` does, in order:
     1. limits and the empty body;
     2. a stored `client_id` returns its receipt;
     3. snapshots via `spawn_blocking`, with `Preparing` progress;
     4. one transaction for the row and its files;
     5. on a conflict, removes its own snapshots and returns the stored receipt.
   - **Ack, Delete, and the accepted-refused branch** of `drop_row`: one transaction
     deletes the row (its file rows cascade) and inserts the snapshot paths into
     `deletions`. After the commit it unlinks them, then clears the journal.
   - **`attempt` for a row with files:**
     1. clear the row's flags (a new attempt, or after Retry);
     2. for each file without a `file_id`, in order:
        - check the epoch;
        - verify once per process; the mark is cleared on a PUT read error;
        - upload, then commit its `file_id`;
        - a permanent error is stored in the file's `error` and fails the row;
     3. check that the ids are distinct;
     4. POST with the ids in order. A first `file.not_attachable` clears the `file_id`s
        and repeats 2 in the same attempt; a second fails the row.
   - **Transient, by code:** network errors, 5xx, `file.no_space`, 408, 429, the
     in-progress, expired and stalled upload codes, `transfer.paused`,
     `NotAuthenticated`/401, and `UnexpectedResponse` (as in `Post::send`).
   - **Cancel:** via the row map (step 2). Checked before each create, between chunks, and
     before the POST. **Retry clears the row's flags** before waking the sender.
   - **Delete:** sets the row's cancel flag through the map **before** taking
     `sender.lock` (the map is a std mutex that is never held across an await). Then as
     above.
   - **Pause:** every row upload races its PUT against `session_changes_from(epoch)`, so
     any change away from the sender's epoch pauses it, not only `None`: a sign-out and a
     sign-in merged into one change still pause.
   - `pending()` fills `files`. Transfer ids are issued once per process per file and
     registered in the row map when issued.
   - Tests, each watched failing under a mutant:
     - the POST carries the ids in order, with the reply target and the body;
     - a lost upload answer resumes by `file_client_id`;
     - a restart after two of three uploads uploads only the third;
     - `not_attachable` once re-creates, twice fails;
     - duplicate ids never reach the POST;
     - a user switch mid-upload: no request under the other user's token;
     - sign-out pauses (the row stays pending) and the next sign-in resumes;
     - a 401 leaves the row pending;
     - a damaged snapshot fails before any PUT;
     - cancel in each file state (not started, uploading, uploaded, before the POST),
       then Retry succeeds;
     - Delete returns while an upload is held;
     - a repeat call leaves no extra snapshot;
     - reconciliation removes orphans and keeps a snapshot being enqueued;
     - a crash between the snapshot write and the commit leaves only an orphan, removed
       at the next open;
     - ack and Delete leave no snapshot files; an unreadable key deletes none;
     - 10 files are accepted, 11 refused, a file over 100 MiB refused, and an empty body
       refused, with nothing written.
5. **Core API:** `send_queued_with_files`, `OutgoingFile`, `SendReceipt`, `QueuedFile`,
   `PendingFile`, the consts, and `Error::Api` codes for the local refusals.
6. **FFI:**
   - The offline records, and `send_queued_with_files`.
   - **Transfer events**, which bindings/apple doesn't export yet: `FfiTransferId`, the
     state (with `Preparing`), `FfiTransferEvent`, and a `TransferEventListener` on a
     `Subscription` (listener.rs pattern). A lagged receiver delivers a `Resync` event;
     the UI re-reads `pending_messages`. `cancel_transfer` is exported too.
   - Tests for the mapping and the lag rule. The Swift package is rebuilt and the Swift
     tests run.

## Where this fails

- **Crypto misuse:** a new key per write, with a test.
- **An upload under the wrong user:** `EpochToken`, the pause on any epoch change, and a
  test.
- **Deadlock:** the flag map is a std mutex, never held across an await. Delete sets
  flags before `sender.lock`.
- **Stale flags:** each attempt and each Retry clears the row's flags, with a test.
- **Runtime starvation:** snapshot I/O runs in `spawn_blocking`.
- **Test speed:** small chunks, with the same format.

## If it stops halfway

- After 1: an unused module with tests. Safe.
- After 2: uploads behave as before, plus row flags and a pause. Safe.
- After 3 without 4: the format is bumped for nothing. Don't merge 3 without 4.
- After 4–5 without 6: core is complete, and the Mac has no progress or cancel. Merge only
  with 6, since the Mac UI needs it.

## Plan review round 1

Taken from the adversarial review: stale flags after a cancel (Retry and each attempt
clear them); the row map reachable from a synchronous `cancel_transfer`; bounded creates;
`spawn_blocking`; cascade plus a journal in one transaction; the final event being the
caller's; pause on any epoch change; transient by code; the tests listed per step; the
FFI transfer events as their own step; the plumbing (store dir, `Net.upload`,
`Arc<Transfers>`); `[u8; 16]`; clearing the verified mark on a PUT error; `snap/` made
only at open; the chunk size as a parameter.

From the second reviewer: pause checked in the transfer loop, best-effort reconciliation,
and ids registered before a transfer starts, all taken. Rejected: "a token swap mid-PUT"
(one PUT is one request with one token, and each retry re-checks the epoch).
