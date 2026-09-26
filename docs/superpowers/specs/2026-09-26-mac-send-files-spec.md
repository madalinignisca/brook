# Sending files from the Mac (#66): spec

Status: spec, closed after review round 1. Review dial: **Standard** (the user picks each file; core snapshots and
uploads them through the outbox, as it already does for GTK; no new core or wire
behaviour).

The Mac sends text through the queue (#167). Files need the queue too
(`sendQueuedWithFiles`), and so local data, and so the provisioning profile (#79). Like
#167, this is built and unit-tested now, and it works as soon as local data does. GTK's
composer (`clients/gnome/src/outgoing.rs`, #66 and #154) is the reference for limits and
wording.

## Done means

1. **Choosing files:**
   - An **Attach** button (paper clip) next to the message box opens an `NSOpenPanel`,
     allowing several files and no folders. #171 gives the app the user-selected scope it
     needs.
   - Files can also be dropped onto the conversation (`.dropDestination(for: URL.self)`).
     Only while the box can be written in, and not while editing a message.
   - Each chosen file is **staged** as a chip below the box: its name and size, with a
     remove button.
   - Staging refuses, with GTK's texts:
     - more than `maxFilesPerMessage()` in one message: "A message can carry up to 10
       files.";
     - an empty file: "<name> is empty.";
     - one over `maxFileBytes()`: "<name> is larger than 100 MiB." (binary units, as GTK);
     - a folder, a package or a device: "Only files can be sent (not folders or
       devices)." (a URL resource check that it's a regular file; core checks the opened
       file again, #156);
     - a file whose access can't be started, or whose size can't be read: "<name>
       couldn't be read."
   - **Security scope runs from staging to sending:**
     - access (`startAccessingSecurityScopedResource`) starts when a file is staged,
       whether from the panel or a drop, since staging already reads its size and type;
     - it's held while the file is staged;
     - it stops when its chip is removed, and after the send call returns, whether it
       succeeded or failed with the files cleared.
2. **Sending:**
   - With files staged, Send calls `sendQueuedWithFiles(channelId, body, replyToId,
     clientId, files)`. Each file carries its name, its content type (from the file's
     `UTType`, else `application/octet-stream`) and a fresh transfer id. The text may be
     empty (#126).
   - The paths are read while the call runs, since core snapshots each file before it
     returns. The access started at staging is still held.
   - It runs off the main thread (large files). The composer shows "Preparing files…",
     and while it does, Send, Attach, drops and chip removal are all disabled, so no file's
     access can end during the call.
   - On success, the text and the chips clear, and the bubbles re-read.
   - On error, everything stays staged, with GTK's `send_error_text`:
     - `outbox.too_many_files`, `outbox.file_too_large`, `outbox.empty_file`,
       `outbox.file_unreadable` ("A file couldn't be read. Is it still there?"),
       `outbox.store`;
     - `outbox.empty_message`, only possible with no text **and** no files (Send is
       disabled then anyway);
     - `local.unavailable`: "Sending files needs this Mac's storage, which isn't
       available yet."
   - The same text, quote and files again after a failure reuse the `clientId`, as for
     text (#162).
   - **Without local data**, Attach is disabled, with the tooltip "Sending files needs
     this Mac's storage". Drops are refused the same way. Text sends as today.
3. **The pending bubble for a message with files** lists each file with its name and
   upload progress. The progress comes from `subscribeTransfers` events for that file's
   transfer id, through the existing `TransferBridge` pattern. The failure text follows
   GTK:
   - `transfer.cancelled`: "Cancelled", with Retry and Delete;
   - `outbox.snapshot_damaged`: "Not sent: a file's saved copy is damaged";
   - `file.*` and `outbox.duplicate_file`: "Not sent: a file was refused". Each file's
     line then gives `file_error_text` (too large for the server, over the quota, a
     refused type, the same file twice, a damaged copy).

   **Cancel** on an uploading bubble calls `cancelTransfer` on one of its transfer ids,
   which cancels the whole message's sending; Retry resumes it.
4. **Tests** (models against `FakeChat`, each watched failing under a mutant):
   - Staging:
     - each refusal and its text, including a file that can't be read;
     - removing a chip ends that file's access;
     - folders and packages are refused.
   - Sending:
     - the files cross with names, types and distinct transfer ids;
     - an empty body is allowed with files;
     - success clears the text and the chips;
     - an error keeps them, and each code gives its text;
     - `local.unavailable` gives its text and keeps everything staged;
     - a file gone between staging and Send (`outbox.file_unreadable`) keeps everything
       staged, with its text;
     - while preparing, chips can't be removed and Attach is disabled;
     - after the call, access ends for every file sent;
     - the same message after a failure reuses its `clientId`, and a changed file set
       gets a new one.
   - Bubbles: the file texts per code; progress per transfer id; Cancel calls
     `cancelTransfer`.
   - Attach is disabled without local data.

## Not doing

- Pasting images from the clipboard, and screenshots dropped as file promises (non-URL
  drops). Those are later.
- Image previews in the composer (the decoder's row UI is after #79).
- Resuming a large upload across app launches beyond what the outbox already does.

## Where it fails

- **A file moved or deleted between staging and Send:** core's snapshot fails with
  `outbox.file_unreadable`, and the text says so, with everything still staged.
- **Security scope for a dropped URL:** access is held for the whole enqueue call. If it
  can't be started, the file is refused at staging ("couldn't be read").
- **A huge file blocking the UI:** the enqueue runs off the main actor, and the composer
  shows it's preparing.

## Review round 1

Vibe; the dial is Standard, so this is the one external review. All five points are taken:
- security-scoped access from staging to sending, both for the panel and for drops;
- an "couldn't be read" staging text;
- `outbox.empty_message` only with no text and no files;
- tests for a file gone before Send and for access ending;
- chips, Attach and drops are locked while the files are being prepared.

Closed.
