# Core: keep the signed-in user current: plan

Spec: `2026-09-26-core-stored-user-spec.md` (closed, Heavy). One PR: core only, with no
binding or client change (clients already take the user from the restore outcome and
the `update_profile` answer).

1. **`persist.rs`: `Persistence::rewrite_user(&self, session: &Session) -> bool`**, modelled
   on `follow_rotation` (load, compare, replace; no fence either way):
   ```text
   if self.fenced() { return false }
   let Ok(Some(stored)) = self.load() else { return false }       // Err or None: skip
   if stored.refresh_token != session.refresh_token { return false }
   replace(name, {user: session.user, refresh_token: stored.refresh_token}).is_ok()
   ```
   A failure is logged at `warn` without the token, and there's no `write_fence` and no
   `lift_fence`.
2. **`session_store.rs`: `SessionStore::replace_user(&self, epoch: u64, user: User) -> bool`**:
   ```text
   let _flight = self.flight().await;                  // refresh lock, then the slot's
   let mut guard = self.cell.write().await;
   let cell = &mut *guard;                             // disjoint field borrows
   if self.is_closed() || cell.rev.epoch != epoch { return false }
   let persisted = cell.persisted;
   let Some(s) = cell.session.as_mut() else { return false };
   if s.user.id != user.id { return false }
   s.user = user;
   if persisted { let _ = self.with_slot(|p| p.rewrite_user(s)); }   // guarded
   true
   ```
   There's no `rev_tx` send and no `state_tx` send.
3. **`install_for_login`** returns `Install::Installed(u64)`, the epoch it set. The code
   that uses it: restore's match (`client.rs`), login's `== Install::Installed` (it becomes
   `matches!(…, Install::Installed(_))`), and two test assertions in `session_store.rs`.
4. **`account.rs`: `update_profile`** takes `epoch` from its snapshot (already read for
   `send`). On success it calls `self.session.replace_user(epoch, out.user.clone())`
   before returning.
5. **`client.rs`: `restore()`**:
   - the task returns `(RestoreOutcome, Option<(u64 epoch, String access)>)` when
     installed;
   - after `task.await`, if installed, **one** `tokio::time::timeout(profile_wait, …)`
     around both `fetch_me(&http, &base, &access)` and the `replace_user(epoch, user)`
     that follows it. A `replace_user` waiting on a long `flight()` is cancelled before it
     takes the cell's lock, which is safe;
   - the outcome becomes `LoggedIn(user)` only when `replace_user` returned true. On
     anything else it's unchanged (and a different id is logged);
   - `fetch_me` exists (`client.rs`), so it's reused;
   - `profile_wait` is a per-client field (default 5 s), set in tests the way
     `locked_bound` and `revoke_wait` are.
6. **The harness** (`test_support.rs`):
   - a per-handle profile override shared by `GET` and a new `PATCH /api/v1/auth/me`
     (applying `display_name` and `status_text`);
   - a `me` mode set after login (`Ok`, `Fail(code)`, `Stall`, `OtherId`), so the login's
     own `me` is unaffected.
7. **Tests**, in `restore_tests.rs` (`InMemoryKeySlot` with `fail_next`, a tempdir
   fence). Each is watched failing under a mutant, and each restore runs under
   `tokio::time::timeout` so a hang fails:
   - after `update_profile`, the slot's bytes hold the new name, and a restore with `me`
     set to `Fail(500)` returns it;
   - a restore returns and stores the server's current name (a profile override set
     since login);
   - `me` set to `Fail(500)`, `Stall` (the restore returns within `profile_wait`) and
     `OtherId`: the stored user is kept;
   - `replace_user` refused on another id, on a stale epoch (after a sign-out; after a
     sign-out and a same-user sign-in) and on a closed store: nothing is written;
   - fenced while signed in (`fail_next("replace")` before login): it stays fenced (the
     file checked), and the family markers are unchanged;
   - a foreign token in the slot (`slot.put`) isn't overwritten; a failed `load`
     (`fail_next("load")`) and an emptied slot skip the write;
   - a failed write (`fail_next("replace")` before `update_profile`): no fence file, and
     `sign_out_complete` is unchanged;
   - a newer client's slot is never written (a second client on the same slot);
   - no `AuthState` change (`has_changed()`) and the same `credential_rev`;
   - `update_profile` running concurrently with a refresh completes.
8. **Docs:** `AuthState::LoggedIn` says its user is the user as of sign-in.
   `RestoreOutcome::LoggedIn` says its user is the server's current one when that could be
   read in time.

**If it stops halfway:** each step compiles, but passes clippy (`-D warnings`,
`dead_code`) only once step 4 uses `replace_user`. So steps 1 to 4 land together. Step 5
then adds other devices' edits. Nothing changes the token path.

**Latency:** `update_profile`'s answer may wait behind a flight holder (up to the request
timeout). That's acceptable for a settings sheet, and it's stated here.

**Where it fails:**
- `update_profile` inside a `flight()` holder would deadlock. It isn't one, since
  `Ctx::send`'s single-flight refresh releases the lock in its own task. A test calls
  `update_profile` concurrently with a refresh.
- The restore test for "a refresh afterwards isn't blocked" is the self-deadlock check.

## Plan review, round 1 (Heavy: vibe and a read-only subagent)

Taken (subagent):
1. **Harness:** a profile override, the PATCH route and a `me` mode, plus the tests
   listed with the knob each needs.
2. **One bound** covers `me` and `replace_user`'s `flight()` wait.
3. **The borrow fix:** read `persisted` first, through a reborrow.
4. **All four `Install::Installed` sites** are named.
5. **The outcome changes only** when `replace_user` succeeds.
6. **`profile_wait`** is a per-client field, not a const.
7. **The clippy note** in "If it stops halfway".
8. **The `RestoreOutcome` doc.**
9. **Each restore test runs under a timeout.**

Confirmed by the subagent: no deadlock (every `flight()` holder was checked), and the
access token after install is safe.

Rebutted (vibe):
1. "A refresh after install makes the epoch stale." A refresh moves only
   `credential_rev`. Only a sign-in or sign-out moves the epoch, and then refusing is
   right.
2. "The lock order could invert." The cell's lock isn't the slot mutex. Every `flight()`
   caller takes refresh, then slot, then cell, which the subagent confirmed site by site.
3. "The failed write result is discarded." That's deliberate (spec §1): a user-only
   failure leaves a valid, restorable record.
4. "Epoch and failure tests are missing." Both are in spec §4 and are now listed in step 7.

Closed.

## Implementation review, round 1 (Heavy: vibe and a read-only subagent)

- **Subagent:** the diff matches the plan, with no correctness bugs. Taken:
  1. **The `rev_tx` watch** is asserted directly (a send with the same values wakes watchers).
  2. **The login's markers:** `marked_current` and the slot's generation (`family()`) are
     unchanged. A `mark_new_login` mutant survived `marked_current` alone.
  3. **The refused other user writes nothing**, asserted where it happens.
  4. **`User`'s doc** no longer claims profile changes reach the UI through the auth watch.
  5. **An unchanged profile skips the keychain round trip** (most restores).
- **Vibe** found one deadlock: "`with_slot` takes the slot's tokio lock with
  `blocking_lock`". Rebutted: `with_slot` goes through `guarded`, which takes only the
  process-wide `OWNERS` std mutex (`persist.rs`). The tests exercise exactly that call
  under `flight()` and finish in milliseconds.

Covered only by reading the code (no test fails without them):
- The restore's own id check. It's an equivalent mutant: `replace_user` refuses another
  id too.
- The outcome changing only when `replace_user` succeeds. No test makes it refuse
  mid-restore.
- `replace_user` taking `flight()`. The cell lock alone would serialise it with a refresh
  commit, and any inversion would only show by timing.
- The timeout covering `replace_user` as well as `me`.
- The closed check. It's caught today only because the spawned sign-out hasn't run yet.

Measured: 16 mutants, all caught once the markers check was added; 2 equivalent or
uncovered, as listed. Core 479, FFI 33 and Mac 261 pass.
