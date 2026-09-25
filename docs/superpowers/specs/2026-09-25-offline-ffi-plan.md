# Offline data over the Apple bindings: plan

Spec: `2026-09-25-offline-ffi-spec.md`. One PR, three commits, each green on its own.

## Steps

1. **Core: state feed, retained loss, cleanup** (`offline.rs`, `client_offline.rs`,
   `client.rs`).
   - `Offline` gets `state: watch::Sender<CacheState>` (owned by the client, like
     `cache_events`) and `generation: Arc<AtomicU64>`. `signed_in` bumps the generation for a
     new or resumed user and starts a state forwarder for that generation, which copies
     the cache's `state()` watch with `send_if_modified(|s| gen == mine && {*s = new; true})`.
     `signed_out`, `close_active` and `forget` bump the generation, then `send_replace(default)`.
     A **same-user sign-in at a new epoch** does the same before resuming: the watcher only
     sees the newest snapshot, so a sign-out and sign-in of one user can arrive as a single
     change, and `signed_out` never runs (review round 1). `Active` records its epoch; any
     epoch change resets the feed and starts a new forwarder.
   - The pump's notice forwarder, and the state forwarder, stop through a drop guard owned
     by the pump task: abort or return, both end.
   - Losses: one client-owned `Arc<Mutex<Losses { newest: u64, unacked: bool }>>`, created
     with the client and shared into `Offline` (never reset with it, so an old number can
     never match a newer loss). Every place that sends `OutboxLost` (open_user's report,
     `enable_local_data`'s reconcile, before `Offline` exists) goes through one
     `record_loss()` that bumps and sets, then sends the event. `outbox_lost() ->
     Option<u64>`; `acknowledge_outbox_lost(n)` clears only if `n == newest`.
   - `cached_channels` / `cached_messages` count rows that failed to parse and log it at
     debug.
   - `send_queued(channel, body, client_id: String)` stays `Option` in core (GTK passes
     `None` today); only the FFI requires it.
2. **FFI: records, methods, listeners** (`bindings/apple/src`).
   - `types.rs`: `FfiCachedChannel`, `FfiMember`, `FfiMessage`, `FfiCachedMessages`,
     `FfiPendingMessage`, `FfiPendingState`, `FfiDeleted`, `FfiCacheEvent`,
     `FfiCacheState`, `FfiLocalUser { origin, user_id }` (UniFFI carries no tuples), with
     `From` impls from core types.
   - `client.rs`: one method per core call, errors through the existing `LoginError` map.
   - `listener.rs`: `CacheEventListener` (broadcast; `Lagged` → `Reset`), and
     `CacheStateListener` via the existing `subscribe_watch`.
3. **Swift package**: `build-xcframework.sh` (arm64 only) regenerates the gitignored
   bindings and framework; `swift build`, and a Swift test that the generated API compiles
   against a mock listener (no network).

## Where this fails

- **A state update after the reset.** Guarded by checking the generation inside the watch's
  lock; the test holds A's update until after the switch.
- **An acknowledge that clears a newer loss.** Guarded by the number; the test records loss
  B between read and acknowledge.
- **A deadlock on the offline mutex from a listener.** Listeners run on runtime threads and
  never hold the offline lock; the state forwarder only touches the watch. A Swift callback
  that calls back into the client (e.g. `cached_messages`) takes the lock after the
  delivery returns control; no lock is held across the FFI.
- **A key-slot callback re-entering the client.** The one exception: wipes and opens call
  the Swift `FfiKeySlot` synchronously while the offline lock (or the session's write lock)
  is held. Changing that boundary is out of scope; instead the contract is written on
  `FfiKeySlot`: its methods must not call back into `BrookClient` or wait on anything that
  does. `KeychainSlot` only calls `SecItem*`, which satisfies it.
- **UniFFI and `SystemTime`.** Mapped to `Option<i64>` unix ms to avoid a timestamp type in
  the Swift surface.
- **Swift regeneration drift.** The generated bindings are gitignored and made by
  `build-xcframework.sh`, so a stale local copy can hide a break: the Swift build runs
  after a fresh script run, never against what was lying there.

## If it stops halfway

- After step 1: core only; the GTK client gains the state feed and loss number, the Apple
  side is unchanged. Safe to merge alone.
- After step 2 without step 3: Rust builds, but nothing has shown the Swift side compiles
  against the new surface. Don't merge without step 3.
