# Offline data over the Apple bindings (C5 follow-up)

Status: spec. Builds on C5 (#109). Review dial: **Standard** (the bindings mirror core; no new
storage, auth or wire behaviour).

## Done means

1. Swift can call every C5 client method through `brook-ffi`:
   `enable_local_data(slot, data_dir) -> Bool`, `cached_channels`, `cached_messages`,
   `load_head`, `load_older`, `send_queued`, `pending_messages`, `retry_send`,
   `delete_pending`, `unsent_count`, `other_local_users`, `wipe_other_local_users`,
   `sign_out_and_forget`.
2. Two listeners, same contract as the auth listener (`Subscription`, cancel, callbacks on a
   runtime thread):
   - `subscribe_cache_events(listener)`: `Channels([id])`, `Removed([id])`, `Users([id])`,
     `Reset`, `Outbox(channel_id)`, `OutboxLost`. A lagged receiver delivers `Reset`
     (re-read everything), never silently skips.
   - `subscribe_cache_state(listener)`: `{syncing, last_synced_unix_ms?, offline}`, latest
     state wins; a user switch or sign-out delivers the default state.
3. Errors stay `LoginError::Api { code }` with the core codes (`local.unavailable`,
   `local.store`, `outbox.*`); Swift switches on the code.
4. Rust tests in `brook-ffi` for the event mapping (every variant), the lag → `Reset` rule, and
   the state listener following a user switch. Each watched failing under a mutant.
5. The Swift package builds (`swift build` in `bindings/apple/swift/BrookCore`) with the
   regenerated bindings.

## Core change needed

`cache_state()` today is a snapshot of the active cache: a listener can't follow it across a
user switch. Add a client-level `watch::Sender<CacheState>` (like `cache_events`), which the
per-user pump forwards into and which resets to default when the stores close. The FFI
subscribes to that.

## Not doing

- The Mac UI (#62): message list, composer, pending rows, the sign-out warning and the
  "other user's data" prompt. Separate PR.
- Turning local data on in the Mac app: it needs the keychain, which waits on the provisioning
  profile (#79). The FFI is callable; the app doesn't call it yet.
- GTK/KDE bindings (they use core directly).
- Attachments and files (#65–#67).
