# Offline data over the Apple bindings (C5 follow-up)

Status: spec, revised after review round 1. Builds on C5 (#109). Review dial: **Standard**
(the bindings mirror core; no new storage, auth or wire behaviour).

## Done means

1. Swift can call every C5 client method through `brook-ffi`:
   `enable_local_data(slot, data_dir) -> Bool`, `cached_channels`, `cached_messages`,
   `load_head`, `load_older`, `send_queued`, `pending_messages`, `retry_send`,
   `delete_pending`, `unsent_count`, `other_local_users`, `wipe_other_local_users`,
   `sign_out_and_forget`, plus the new `outbox_lost` / `acknowledge_outbox_lost`.
2. **`send_queued` takes a required `client_id`.** Swift makes it (a UUID) and keeps it
   before calling, so an error whose answer was lost can always be retried with the same
   id: the id never exists only inside a failed call.
3. The records carry what the UI draws:
   - `FfiCachedChannel`: id, kind, name, archived, **unread count**, **members** (id,
     handle, display name: DM titles).
   - `FfiCachedMessages { messages, needs_network }`, newest first.
   - `FfiMessage`: id, channel id, author id/handle/display name, body, created at,
     **client id**, **deleted** (a tombstone keeps its place, body empty).
   - `FfiPendingMessage { client_id, channel_id, body, state }`, with state `Pending |
     Sending | Accepted | Failed { code }`.
   - `FfiDeleted`: `Removed | AlreadySent | NotFound`.
4. Two listeners, same contract as the auth listener (`Subscription`, cancel, callbacks on a
   runtime thread):
   - `subscribe_cache_events(listener)`: `Channels([id])`, `Removed([id])`, `Users([id])`,
     `Reset`, `Outbox(channel_id)`, `OutboxLost`. A lagged receiver delivers `Reset`
     (re-read everything), never silently skips. Events are hints to re-read; nothing
     depends on receiving one (see 5).
   - `subscribe_cache_state(listener)`: `{syncing, last_synced_unix_ms?, offline}`, latest
     state wins. **Signing out, a user switch, a wipe and the client closing each deliver
     the default state**, and nothing from the previous user's cache reaches it after.
5. **Lost unsent messages are retained state, not only an event.** Core keeps a
   `outbox_lost` flag (set by `OutboxLost` from any source, including during
   `enable_local_data` before anyone subscribed) until `acknowledge_outbox_lost()`. The
   app reads it on start and after any `OutboxLost`/`Reset` event.
6. Errors stay `LoginError::Api { code }` with the core codes (`local.unavailable`,
   `local.store`, `outbox.*`); Swift switches on the code (2 makes the missing id moot).
7. Tests, each watched failing under a mutant:
   - `brook-ffi`: the event mapping (every variant), lag → `Reset`, record mapping (unread,
     members, client id, tombstone, every pending state and `Deleted` outcome).
   - core: `outbox_lost` survives until acknowledged and is set when the loss happens
     before any subscriber; the state feed goes to default on sign-out, switch and close; a
     state update from user A's cache held back until after the switch to B never
     overwrites B's state.
8. The Swift package builds (`swift build` in `bindings/apple/swift/BrookCore`) with the
   regenerated bindings.

## Core changes needed

- **Client-level cache state.** `Offline` owns a `watch::Sender<CacheState>` and a
  generation counter. Each user's forwarder writes with `send_if_modified`, checking its
  generation *inside* the closure (the watch's lock): a switch or sign-out bumps the
  generation, then resets the state through the same lock, so a late forward from the old
  cache either lands before the reset (and is overwritten) or sees the new generation and
  is dropped.
- **The pump's forwarder is stopped with it.** Aborting the pump today skips the line that
  aborts its notice forwarder; a drop guard does it however the pump ends.
- **Debug log** of rows `cached_channels` / `cached_messages` drop as unreadable (from the
  Linux review of #109).

## Not doing

- The Mac UI (#62): message list, composer, pending rows, the sign-out warning and the
  "other user's data" prompt. Separate PR.
- Turning local data on in the Mac app: it needs the keychain, which waits on the provisioning
  profile (#79). The FFI is callable; the app doesn't call it yet.
- GTK/KDE bindings (they use core directly).
- Attachments and files (#65–#67).
