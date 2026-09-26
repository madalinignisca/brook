# One refresh at a time per stored session: plan

Spec: `2026-09-26-refresh-single-flight-spec.md` (closed after round 2). Heavy. One PR.

## Steps

1. **`persist.rs`**:
   - `Persistence::owns(owner) -> bool`, a read of `OWNERS` with no keychain call;
   - `slot_lock(&self) -> Arc<tokio::sync::Mutex<()>>` from a static map keyed by slot
     name, one per slot, process-wide.
2. **`session_store.rs`**:
   - The cell gains `from_slot: bool`: the current login was restored from, or written to,
     the slot while this store owned it. It's set at `install_for_login` and
     `install_for_challenge` when the slot write succeeded, and cleared when a session is
     installed without one. Unlike `persisted`, a sign-out doesn't clear it: it describes
     the login, not the stored copy.
   - `sign_out` and `take_out_for_login` return a `Taken { session, from_slot }` instead of
     a bare `Session`. `commit_refresh`'s `Discarded` carries `from_slot`.
   - `owns() -> bool` is true when persistence is off (review round 2, N2), and otherwise
     `Persistence::owns(self.id)`.
   - `spare(from_slot) -> bool` is `from_slot && persistence is on && !owns()`.
     `revoke_detached(token, from_slot)` does nothing when `spare` is true. Every caller
     passes where the token came from:
     - logout and the displaced session of a login: `Taken.from_slot`;
     - `refresh_once`'s and account's discarded pairs: the `Discarded` flag;
     - restore's `Stale` and `SlotTaken`: true;
     - a fresh login's or challenge's pair that never installed: false;
     - `close_detached`: the cell's flag (it only revokes with persistence off, so
       nothing changes there).
   - `slot_lock()`: the persistence's lock, or `None` without persistence.
3. **`client.rs` / `account.rs`**:
   - Right after every `refresh_lock` acquisition (restore, `Refresher::refresh`, the two
     account sections), take `session.slot_lock()` too, and hold both.
   - `refresh_once` first checks `session.owns()`. If false, it sends nothing, signs out
     locally with `sign_out(false)`, drops the taken session (spared, never revoked), and
     answers `NotAuthenticated`.
4. **Tests** (`restore_tests.rs`), each watched failing under a mutant:
   - **the dropped client:** A signs in, is dropped (it stays the owner), then B claims the
     slot and restores. A's refresh loop is held in flight and released after B restores.
     The server records no logout of the family, and B's token is still live.
   - `logout()` on a superseded client: no logout request, B stays signed in.
   - a superseded client's refresh sends nothing and ends its session with `LoggedOut`.
   - a fresh login refused with `SlotTaken` still revokes its pair.
   - a client without persistence refreshes and revokes as before (the N2 guard).

## Where this fails

- **Deadlock:** the order is always `refresh_lock`, then the slot lock. `sign_out` takes
  only the cell lock, and `owns()` holds `OWNERS` for a map read only. (The review
  checked this.)
- **Legitimate revokes skipped:** `spare` needs all three conditions, and fresh pairs pass
  `from_slot = false`. There's a test for each.

## If it stops halfway

- Step 1 alone is unused. After step 2 but not step 3, revokes are spared but refreshes
  aren't stopped yet. That's already safer than today, but don't merge it without step 3.
