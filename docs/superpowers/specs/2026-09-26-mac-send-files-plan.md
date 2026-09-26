# Sending files from the Mac: plan

Spec: `2026-09-26-mac-send-files-spec.md` (#172, closed). Standard. One PR.

1. **`Chat/Staging.swift`:** `StagedFile` (url, name, size, content type, transfer id) and
   a pure `stage(url:already:) -> Result<StagedFile, String>`.
   - It starts security-scoped access, then reads `isRegularFile`, `fileSize` and
     `contentType` from resource values.
   - Any refusal stops the access it started, and the texts follow the spec.
   - `StagedFile.release()` stops access. It's idempotent.
   - A small `FileAccess` protocol lets tests fake the scope, the resource values and the
     access count.
2. **`OfflineClient`** gains
   `sendQueuedWithFiles(channelId:body:replyToId:clientId:files:) -> FfiSendReceipt`.
   `FakeChat` records it, with a gate to hold it (the "preparing" tests) and a failure to
   return.
3. **`ComposerModel`:**
   - `staged: [StagedFile]`, `preparing: Bool` and `canAttach: Bool` (a cache is present,
     and not preparing or editing);
   - `attach(urls:)`, `remove(file)` (refused while preparing);
   - `send()` with files: it calls `sendQueuedWithFiles` from a detached task (off main),
     with `preparing = true`.
   - The draft id (#162's rule) is keyed on the text, the quote and the files' URLs.
   - On success it releases every file and clears. On error it keeps everything, with
     GTK's `send_error_text`.
   - `local.unavailable` sets `canAttach = false` for the rest of the session.
4. **`PendingModel`:**
   - the file failure texts and `fileLine(file, progress:)`;
   - `cancel(message)`, which calls `cancelTransfer` on the message's first file transfer
     id;
   - progress per transfer id from `subscribeTransfers`, through a `TransferBridge`-like
     bridge on the pending model.
5. **Views:**
   - an Attach button (`NSOpenPanel`: files only, several at once);
   - `.dropDestination(for: URL.self)` on the conversation, active when `canAttach`;
   - chips with remove buttons, and "Preparing files…";
   - the bubble's file lines and Cancel.
6. **Tests:** one per spec bullet (§4), each watched failing under a mutant, with
   `FakeFileAccess` for the scope.

**Where it fails:** `URLResourceValues.isRegularFile` on a package (an `.app` is a
directory) answers false, so it's refused, which is right. A symlink to a file is resolved
by the resource values, and it's accepted, as core does.

**If it stops halfway:** steps 1 and 2 are unused. After 3 without 5, nothing visible
changes. Nothing is ever staged without a way to clear it, since remove comes in step 3.

## Plan review, round 1 (vibe; Standard)

Taken:
- **A URL already staged is not staged again.** Core would refuse it anyway, as
  `outbox.duplicate_file`.
- **`stage` stops the access it started on every refusal path** (`defer`, unless it
  succeeds).
- **`preparing` is set and reset only on the main actor, on every path.** The detached task
  only calls core.
- **The draft id also keys on each file's size and modification date,** so a file replaced
  at the same path is a new message and never gets the old one's stored receipt.
- **Tests:**
  - `FakeFileAccess` counts starts and stops, and every path ends balanced (refusal,
    remove, success);
  - a changed file at the same URL gets a new id.

Rebutted:
- **"Release on a send error."** The spec keeps a failed message's files staged for the
  retry, and a staged file holds its access until it's removed or sent. Releasing on error
  would break the retry.
- **"Progress wiring missing."** Step 4 ties `PendingModel` to `subscribeTransfers` by
  transfer id.

Closed.
