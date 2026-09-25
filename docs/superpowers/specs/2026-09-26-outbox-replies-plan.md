# Outbox: queued replies (#111)

Spec: issue #111. Review dial: **Heavy**. The outbox format bump discards format-1 outboxes
at upgrade, and the GTK beta built from main on 2026-09-26 has them, so this deletes local
data (surfaced as lost).

## Done means

1. `send_queued(channel, body, reply_to_id: Option<String>, client_id)` in core; the FFI's
   `send_queued` gains `reply_to_id: Option<String>`.
2. The row keeps `reply_to_id`; every send of it (queued, retried, resent after a lost answer,
   and the direct send when the outbox can't be written) posts it.
3. `PendingMessage` (and `FfiPendingMessage`) carries `reply_to_id`, so the bubble can say
   "Replying to …".
4. A reply whose quoted message is gone is refused by the server (404) and becomes
   `Failed { code }` with Retry and Delete, like any refusal. No special case.
5. The outbox format goes to 2. A format-1 outbox is surfaced as lost (a numbered loss, as
   in #113), then rebuilt, as the pre-1.0 rule says (no local migrations).
6. Tests, each watched failing under a mutant: the reply id reaches the server on the
   first send, on a resend after a lost answer, and on a retry after a refusal; the direct
   send carries it; pending rows show it; a format-1 outbox opens as a loss and then works.

## Plan

- `outbox.rs`: `Post::send` takes a `&Outgoing { body, reply_to_id }`, not a bare body, so
  attachments (next) add a field instead of another parameter. `enqueue`, `send_direct`,
  `next_row` and `attempt` carry it through. The same-id conflict check is unchanged: the
  stored row wins, reply id included, as the body does now.
- `store.rs`: `OUTBOX_V2` = V1 plus `reply_to_id TEXT`; `Kind::Outbox` format 2.
- `cache_http.rs`: `Http::send` puts `reply_to_id` in the JSON only when set.
- `client_offline.rs`, `bindings/apple/src/offline.rs`: the new parameter and field.

## Where this fails

- **A resend without the reply id.** The server answers a resend with the stored message,
  so a first send that lost the id can't be fixed by a later one. Guarded by one code path
  (`next_row` reads it for every attempt) and the resend test.
- **An old outbox read as the new format.** `format_of` compares the stored format, and a
  mismatch on the outbox is `NeedsRebuild`, surfaced before the rebuild. The test opens a
  real format-1 file.
- **A GTK build against the new signature.** GTK calls `send_queued(ch, body, None)` in the
  same workspace. The PR updates that call site mechanically (`None` for the reply), so
  main never breaks; it can't be built on the Mac, so CI's GNOME build is the check. Using
  the new argument for queued replies is the Linux side's follow-up.

## If it stops halfway

- Schema bumped without the send path: rows keep a reply id that is never posted. Replies
  go out as plain messages, which is wrong but not lossy. Don't merge that state; the
  pieces land in one commit.
