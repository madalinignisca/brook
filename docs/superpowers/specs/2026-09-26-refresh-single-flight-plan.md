# One refresh at a time per stored session: plan

Spec: `2026-09-26-refresh-single-flight-spec.md` (closed after round 2). Heavy. One PR.

## Steps

1. **`persist.rs`**:
   - `Persistence::owns(owner) -> bool`, a read of `OWNERS` with no keychain call;
   - `slot_lock(&self) -> Arc<tokio::sync::Mutex<()>>` from a static map keyed by slot
     name, one per slot, process-wide.
2. **`session_store.rs`**:
   - `Persistence::write` returns whether the replace succeeded (today it returns `()` and
     fences on failure). The cell gains `from_slot: bool`, set at `install_for_login` and
     `install_for_challenge` to **whether that write landed**: a login whose keychain write
     failed is fenced and was never stored, so it isn't the owner's family and is still
     revoked (round 1). It's cleared when a session is installed without persistence.
     Unlike `persisted`, a sign-out doesn't clear it: it describes the login, not the
     stored copy.
   - `sign_out` and `take_out_for_login` return a `Taken { session, from_slot }` instead of
     a bare `Session`.
   - `owns() -> bool` is true when persistence is off (review round 2, N2), and otherwise
     `Persistence::owns(self.id)`.
   - `spare(from_slot) -> bool` is `from_slot && persistence is on && !owns()`.
     `revoke_detached(token, from_slot)` does nothing when `spare` is true. Every caller
     passes where the token came from:
     - logout and the displaced session of a login: `Taken.from_slot`;
     - `refresh_once`'s and account's discarded pairs: the flag **read together with the
       token that was sent** (`with_session` reads token and flag in one go). It is not
       read at commit time: by then another login may be installed, and its flag would
       judge the wrong token (round 1);
     - restore's `Stale` and `SlotTaken`: true;
     - a fresh login's or challenge's pair that never installed: false;
     - `close_detached`: the cell's flag (it only revokes with persistence off, so
       nothing changes there).
   - `slot_lock()`: the persistence's lock, or `None` without persistence.
3. **`client.rs` / `account.rs`**:
   - Right after every `refresh_lock` acquisition (restore, `Refresher::refresh`, the two
     account sections), take `session.slot_lock()` too, and hold both.
   - `refresh_once` keeps its `NoSession` check first. Only then does it check
     `session.owns()`, so a client with no session never signs out on every poll (which
     would kill an in-flight login). If the check fails, it sends nothing, signs out
     locally with `sign_out(false)`, drops the taken session (spared, never revoked), and
     answers `NotAuthenticated`.
4. **Tests** (`restore_tests.rs`), each watched failing under a mutant. The TestServer
   has no family, grace or reuse model: `logout` removes one token, and `Strict` refuses a
   rotated token. So the assertion that catches the bug is **`server.logouts()` stays
   empty**, and the tests use `Rotate` mode.
   - **the dropped client**, in the order the gate allows (`gate_refresh` holds every
     refresh answer, and B's restore would wait on the slot lock A's refresh holds):
     1. A signs in with persistence.
     2. A's refresh is started through `Refresher::refresh` (the path the loop and the
        WebSocket 1008 recovery share), as its own task, under the gate.
     3. A is dropped.
     4. B is created with persistence, which claims the slot.
     5. A permit lets A's answer through, and A's task is awaited.
     6. Check that `server.logouts()` is empty. Then B restores and is signed in.
   - `logout()` on a superseded client: no logout request.
   - a superseded client's `Refresher::refresh` sends no request and ends its session with
     `LoggedOut`. It's the one function the loop and the 1008 recovery both use.
   - a fresh login refused with `SlotTaken` still revokes its pair.
   - a login whose keychain write failed (`fail_next("replace")`), then superseded, still
     revokes on logout.
   - a client without persistence refreshes and revokes as before (the N2 guard).
   - Not buildable without a family model on the TestServer: "two live clients, no reuse
     recorded". The logouts assertion covers the revoke half. The refresh half (a
     superseded client sends nothing) has its own test above.

## Where this fails

- **Deadlock:** the order is always `refresh_lock`, then the slot lock. `sign_out` takes
  only the cell lock, and `owns()` holds `OWNERS` for a map read only. (The review
  checked this.)
- **Legitimate revokes skipped:** `spare` needs all three conditions, and fresh pairs pass
  `from_slot = false`. There's a test for each.

## If it stops halfway

- Step 1 alone is unused. After step 2 but not step 3, revokes are spared but refreshes
  aren't stopped yet. That's already safer than today, but don't merge it without step 3.

## Plan review round 1

Taken: `from_slot` from whether the keychain write landed; the flag read together with the
token that was sent, not at commit; the test order the gate allows, using `logouts()` as
the assertion that catches the bug (the TestServer has no family model); `NoSession`
checked before ownership; the refresh tests go through `Refresher::refresh`, the path the
loop and the 1008 recovery share.
