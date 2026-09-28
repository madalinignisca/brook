# Core: keep the signed-in user current (#184's known gap): spec

Status: spec, closed after review round 2. Review dial: **Heavy**. It writes the stored session, the
secure-store record that holds the refresh token.

## The problem

The signed-in `User` is fetched once, at a password sign-in, and stored with the refresh
token (`persist::Stored { user, refresh_token }`). `restore()` reuses `stored.user` and
never asks again. So a name or status change keeps showing the old one:
- after `update_profile` on this device (the app shows the answer, but only until
  relaunch);
- after an edit on another device, forever, until the next password sign-in.

## Done means

1. **`SessionStore::replace_user(epoch, user) -> bool`**. It replaces the in-memory
   session's user, and re-stores it when that's safe.
   - **Locks**, in the order every writer uses: `flight()` (the refresh lock, then the
     slot's), then the cell's write lock. The caller must not hold `flight()`: restore
     calls it after its own flight has ended (item 3).
   - **Refused, changing nothing**, when any of these holds: the store is closed
     (`is_closed()`, as every writer checks); the epoch moved (a sign-out or a new
     sign-in); there's no session; `user.id` differs from the session's.
   - **The store write is a new `Persistence::rewrite_user(&Session) -> bool`**, never
     `write`:
     - It runs only through the slot guard (`with_slot`, owner only) and only while
       `cell.persisted`.
     - It skips (fails closed) when the slot is **fenced**, so it never lifts a fence.
       (`write` lifts it, which could expose a token no family marks, #140.)
     - It skips when the stored record's refresh token isn't the session's current one
       (a read first), and when the read fails or finds nothing (`Err`: locked or
       unreadable; `Ok(None)`: cleared on purpose, never to be brought back). This guards
       against this client's own stale state. A newer client is kept out by the owner guard,
       which holds the process-wide `OWNERS` mutex across the read and the write.
     - Otherwise it replaces the record with `{user, same refresh_token}`.
     - **A failed write fences nothing and is ignored** (logged). The stored copy still
       holds the same valid token, with only an older name.
   - **Untouched:** tokens; `credential_rev` and `rev_tx` (no socket re-auth);
     `login_gen`; `refresh_not_before`; `persisted`; `sign_out_complete`; the
     family/new-login markers (`mark_family`, `mark_new_login` and `new_family` are never
     called); and `AuthState` (see "Where it fails").
2. **`update_profile`** calls `replace_user` with the epoch its request was sent in and
   the server's answer, before returning it.
3. **`restore()`**:
   - `install_for_login` returns the epoch it created (`Install::Installed(epoch)`).
   - After the restore task (and its `flight()`) has ended, and only if it installed,
     `restore` sends `GET /auth/me` with the installed access token, **bounded to 5 s**.
   - On success with the same user id: `replace_user(installed epoch, user)`, and
     `RestoreOutcome::LoggedIn` carries that user. The replace is refused if a sign-out
     or a new sign-in came meanwhile.
   - On anything else it signs in with the stored user: a timeout, offline, a 5xx, a 401
     (the background refresh deals with expiry), or a different id (logged).
4. **Tests** (each watched failing under a mutant):
   - `update_profile` re-stores the new user, checked in the slot's bytes, and a restore
     whose `me` fails returns the stored new name. (A restore whose `me` works would hide a
     missing write);
   - a failed or empty read skips the write (a cleared slot stays cleared);
   - restore returns and re-stores the server's current user, and a refresh afterwards
     isn't blocked (no self-deadlock);
   - restore signs in with the stored user when `me` fails (500), stalls (it returns
     within the bound), or names another id;
   - `replace_user` refuses another id, a stale epoch (a sign-out; a sign-out then a
     same-user sign-in) and a closed store, and writes nothing;
   - a fenced slot stays fenced (the fence is checked on disk), and the markers are
     unchanged;
   - a stored record holding a different refresh token isn't overwritten;
   - a failed write leaves no fence, and `sign_out_complete` is unchanged;
   - a newer client's slot is never written;
   - no `AuthState` is published and `credential_rev` doesn't move.

## Not doing

- Live updates of your own profile from another device mid-session (`/sync users`). The
  next launch picks them up.
- Any change to `AuthState` or to the clients' auth handling.

## Where it fails

- **`AuthState` is the user as of sign-in.** After `replace_user`, `state()` still holds
  `LoggedIn(old user)`, while `session.user` (`current_user_id`, `is_admin`) is current,
  and so is the restore outcome. Re-publishing `LoggedIn` could restart a client's
  signed-in UI, so it isn't done. Clients take the user from the restore outcome or the
  `update_profile` answer. Documented on `AuthState::LoggedIn`.
- **Ordering:** two `update_profile`s, or one racing restore's `me`, are ordered by
  `flight()`, not by the server, so the stored name can briefly be older than the
  server's. The next launch's `me` corrects it.
- **A slot write that failed earlier** (fenced) isn't rewritten, so the old name stays
  stored until a token write lifts the fence properly. That's a name, never a token.
- **The launch cost:** at most 5 s more before the restore outcome, only when `me`
  stalls. `LoggedIn` is already published before that, with the stored user.

## Spec review, round 1 (Heavy: vibe and a read-only subagent standing in for codex)

Taken (the subagent's 9):
- **The restore self-deadlock:** `me` and `replace_user` run after the task's `flight()`.
- **A new write path:** no fence on failure, no lifting a fence, and no marker changes.
- **The `is_closed()` check.**
- **Restore's epoch comes from `install_for_login`.**
- **A 5 s bound on `me`**, tested with a stall.
- **`AuthState` staleness** is stated and documented.
- **The ordering note.**
- **The untouched list.**
- **The added tests.**

Rebutted (vibe 1): "the lock order is inverted versus `commit_refresh`". `commit_refresh`
is only ever called by a holder of `flight()` (the refresh path), then takes the cell's
lock, which is the order here. Vibe 2 and 3 (the fence) are the same as the subagent's
point, taken.

## Spec review, round 2

- **Vibe:** no findings.
- **Subagent:** no race. The owner guard holds `OWNERS` across `rewrite_user`'s read and
  write, and `claim()` needs the same mutex. All round-1 holes are closed.
- **Taken:**
  - a failed or empty read skips the write;
  - the item-2 test checks the slot's bytes, and a failing `me`, so it can't pass
    without item 2;
  - the spec now names the owner guard, not `flight()`, as what keeps a newer client out.
- **Noted:** one more keychain round trip per profile update, under the same locks as
  the existing writers.

Closed.
