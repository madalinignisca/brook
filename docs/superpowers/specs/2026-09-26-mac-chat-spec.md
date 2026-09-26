# The Mac chat surface: timeline, composer, attachments (#62, #66)

Status: spec. Review dial: **Standard** (bindings mirror core; a UI; no new storage, auth
or wire behaviour).

## Why this is first

The Mac app has sign-in, the channel list, calls and account settings, but **no message
view**. #66 (attachments) and #62 (offline) both hang off a timeline, so this builds it.
Core already has everything: `channel_history`, `send_message`, `edit_message`,
`delete_message`, the message events, `download_file`. The bindings don't expose them yet.

## Done means

1. **Bindings (brook-ffi):**
   - `FfiMessage` grows what a timeline draws: `edited_at`, `reply_to_id`, a `reply_to`
     excerpt (`id`, author name, body, `deleted`, `attachments`), `attachments:
     [FfiFileInfo]` (id, `filename` to save under, `original_name` to show, size,
     content type), plus the fields it has.
   - `channel_history(channel, before?) -> [FfiMessage]` (oldest first, as core);
     `send_message(channel, body, reply_to_id?)`; `edit_message`; `delete_message`;
     `mark_read`.
   - `FfiServerEvent` gains `MessageNew(FfiMessage)`, `MessageUpdate(FfiMessage)` and
     `MessageDelete { channel_id, message_id }`.
   - `download_file(transfer_id, file_id, sha256, size, destination_path)`: core's
     `download_file` into a `FileSink` on the chosen path (a failed or cancelled download
     leaves no file), with progress on `subscribe_transfers`.
2. **Timeline (Mac):** selecting a channel shows its messages, newest at the bottom.
   - Loads the newest page on open, and older pages when scrolled to the top.
   - Live: new messages append; edits update in place; a delete leaves "Message deleted".
   - Each message shows its author, time, an "edited" mark, and its quote ("↳ Replying
     to …", reading "a deleted message" or "a file" from the excerpt's state).
   - A message with files and no text shows only its files.
   - Marks the channel read when shown.
3. **Composer (Mac):** a text field with Send (Return; Shift-Return for a new line), a
   Reply action on a message, Edit and Delete on your own. With local data off it sends
   directly (`send_message`); the text comes back on failure, with the reason.
4. **Attachments (Mac, #66 show and Save):** each file is a row: name, size, a type
   icon, and **Save…** (an `NSSavePanel` defaulting to `filename`), with progress and
   Cancel while saving, and a clear error ("No longer available" for `file.gone`).
   Nothing is opened or previewed inline yet (that's #67's Open).
5. **Tests:**
   - brook-ffi: the new records and events mapping, each field (mutation-checked).
   - Mac: view-model tests (the timeline merging history and live events, deduplicating by
     id, newest at the bottom; the composer restoring text on failure), without a server.
   - A live check against the LAN test server: send, edit, delete, reply, and save a file.

## Later, not in this spec

- **#62 offline** (cached reads first, queued sends and pending bubbles, the offline
  banner, sign-out with "Remove this device's data", lost-message notices): needs local
  data on, which needs the Keychain, which waits on the provisioning profile (#79). It
  builds on this timeline, as its own spec.
- **Sending files from the Mac:** needs the outbox (local data), so it follows #62.
- Open and inline previews (#67), reactions, mentions, typing indicators, drag and drop.
