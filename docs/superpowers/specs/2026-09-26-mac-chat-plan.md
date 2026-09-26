# The Mac chat surface: plan

Spec: `2026-09-26-mac-chat-spec.md`. Standard. Two PRs.

## PR 1: bindings

- `types.rs` / `offline.rs`: `FfiMessage` gains `edited_at`, `reply_to_id`, `reply_to:
  Option<FfiReplyExcerpt>` and `attachments: Vec<FfiFileInfo>`. The one `From<Message>`
  serves cached and network messages alike.
- `client.rs`: `channel_history`, `send_message`, `edit_message`, `delete_message`,
  `mark_read`, and `download_file(transfer_id, file_id, sha256, size, path)`, which opens a
  `FileSink` on the path (core removes it on failure).
- `call.rs`: `FfiServerEvent` gains `MessageNew`, `MessageUpdate` and `MessageDelete`. The
  existing listener forwards them, since today it drops every other event.
- Tests: mapping for each new field and event (mutation-checked). The Swift package
  rebuilds and a Swift test calls the new functions with no server.

## PR 2: the Mac timeline

- `Chat/TimelineModel.swift` (`@Observable`, main actor):
  - messages keyed by id, ordered by id (UUIDv7, so time order);
  - `load()` gets the newest page, and `loadOlder()` pages before the oldest id;
  - `apply(event)` handles new, update and delete for its channel; an id already present is
    replaced, never duplicated, so a history page and a live event racing is harmless.
- `Chat/ComposerModel.swift`: the text, the reply target, and `send()`, which clears at once
  and restores the text and reply on failure, with the reason. Edit and delete.
- `Chat/TimelineView.swift` / `MessageRow.swift` / `AttachmentRow.swift`: the views, with an
  `NSSavePanel` for Save and progress from `subscribeTransfers`.
- `SignedInView`: the detail pane becomes the timeline, with the call button in its toolbar.
- Tests: `TimelineModel` merging, ordering and dedupe, and `ComposerModel`'s restore on
  failure, against a fake client protocol.
- Check: a live run on the LAN test server (send, edit, delete, reply, save a file).

## Where this fails

- **Events before the first page:** they're applied to the empty model, and the page then
  merges by id.
- **Sending twice after a network error:** a direct send has no `client_id` today, so
  resending after an ambiguous failure could duplicate the message. The composer restores
  the text only on a definite refusal. On a network error it says "may not have been sent"
  and leaves it to the user, until #62 routes sends through the outbox.
