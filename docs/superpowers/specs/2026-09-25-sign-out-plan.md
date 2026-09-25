# Sign out — implementation plan

> Status: **approved** (Heavy, two rounds) · 2026-09-25 · Implements the approved spec
> [2026-09-25-sign-out-design.md](2026-09-25-sign-out-design.md) (Heavy). Tests first, each seen
> failing under a named mutation.

### P1 — Test server
- `/auth/logout` (revokes one refresh token, idempotent, recorded) — already on the TOTP branch's
  fake; brought over. **Gates after issuance** for `/auth/refresh` and `/auth/login` (next to the
  existing password gate), and `live_refresh_tokens(handle)` for the "exactly the winner" checks.

### P2 — Core (lands as one unit: spawn boundaries, lock ownership, drop fencing and cleanup
### only make sense together)
1. `revoke_detached(runtime, http, base, refresh_token)`: spawns through a **runtime handle the
   client retained at construction** (never ambient `tokio::spawn`: a Swift thread releasing the
   client is outside any entered runtime, and an ambient spawn there panics), owning clones,
   `POST /auth/logout`, bounded by the request timeout, errors logged by kind.
2. **Minting operations are whole spawned tasks that own their lock:** `Refresher::refresh`
   (single-flight check, lock, request, commit-or-revoke) and `login` (lock, displace, request,
   install-or-revoke) each run entirely inside one spawned task that acquires the refresh lock
   itself, exactly as `change_password` does; the caller only awaits the join handle. No lock is
   held by a caller around a child that needs it (no deadlock), and a cancelled caller leaves the
   task, lock and all, to finish (single-flight intact).
3. Revocation rules: `commit_refresh` → `Discarded`, a superseded login install and
   `change_password`'s `Discarded` each `revoke_detached` the new pair's refresh token. **The
   session login displaces is revoked when it is taken out**, whatever the new login's outcome.
   **Login reserves its generation before it spawns or waits for the lock**; `logout` and a newer
   login invalidate it, so a login still queued on the lock when `logout` runs installs nothing
   when it finally runs (its pair, if any, is revoked).
4. `logout()`: take the session out (`replace(None)`) and publish `LoggedOut` without the lock,
   then `revoke_detached` the taken token. No session: publish only.
5. **Drop fences the store:** dropping the `BrookClient` closes its `SessionStore` (a flag under
   the store's write lock) and revokes the session it held (with no persisted session, a dropped
   client is a signed-out one). Any install or commit that arrives after the close is refused
   and its pair revoked, so a detached login or refresh finishing after the drop leaves nothing
   live.
6. **Loop lifetime:** the refresh loop and the WebSocket task select on a shutdown signal (a
   watch whose sender the client owns) in **every** wait: the refresh sleep, the reconnect
   backoff, the socket read and the reply waits. Minting work already lives in its own tasks
   (2), so waking a loop never cancels one.
- FFI: `logout()` on `FfiBrookClient`.
- **Check:** spec §5 core tests, plus, each with a named mutation that turns it red:
  - cancel a refresh's caller while its response is gated, then a second refresh: exactly one
    rotation, no orphan (mutation: lock left in the caller);
  - the same for login cancellation and for a login gated then its client dropped (mutation: the
    login's install in the caller's future);
  - displacement by a failed and by a cancelled login still revokes the old token (mutation:
    revoke only on success);
  - drop without logout while a refresh, a login and a password change are each gated: after the
    responses land, no refresh token of the user is live (mutation: no close fence);
  - task exits observed directly (the tasks' join handles, exposed to tests): after a drop the
    loops end while idle-sleeping, while connected and idle on the socket, and during a backoff
    (mutation: shutdown checked only between iterations);
  - logout taking the refresh lock (a stalled refresh then blocks it);
  - a login queued behind a held refresh lock, then `logout`, then the lock released: no session,
    no live token (mutation: generation reserved after the lock, or not checked);
  - the client dropped on a plain thread outside the runtime: no panic, and the revoke arrives
    (mutation: ambient `tokio::spawn` in the cleanup path);
  - shutdown while a loop awaits a minting task's completion (refresh loop, and the WebSocket's
    refresh-before-reconnect): the loop's join completes **before** the response gate is released;
    then the gate is released and the issued token is revoked (mutations: no shutdown selection
    at that wait; shutdown cancelling the minting task).

### P3 — macOS
- `SessionStore`: attempt ids; **one ordered stream** of core auth events per attempt (the
  observer only yields into an `AsyncStream`; a single main-actor task consumes it in order), so
  `LoggedIn` is always handled before a following `LoggedOut`; a `LoggedOut` after `LoggedIn` is
  terminal; the first transition out of signed-in wins; Sign Out ends the attempt before calling
  `logout`. Account menu item. The call model's end path and sheet dismissal on leaving
  signed-in.
- **Check:** spec §5 macOS tests with a fake client scripting auth events in both orders relative
  to the login completion, including `LoggedIn`+`LoggedOut` delivered before it; mutations:
  observer ignored (the remote-sign-out test turns red); events consumed unordered (the
  LoggedIn/LoggedOut ordering test turns red).

### P4 — Live
- itest: sign in, sign out, the old refresh token refused (raw `/auth/refresh` → 401). Required by
  name in `itest.sh`.

## Where this fails
| Point | Failure | Response |
|---|---|---|
| P2.2 | lock ownership split between caller and task | the whole operation and its lock live in the task (as `change_password`); tested with cancellation then a second refresh |
| P2.5 | a commit into a dropped client's store | the store's close fence refuses it and revokes the pair |
| P2.6 | a loop blocked in a wait never sees shutdown | every wait selects on the shutdown signal; exits observed by join handle |
| P3 | events handled out of order across main-actor hops | one ordered stream, one consumer |

## If it stops halfway
P1 is test-only. **P2 lands as one unit:** moving work into tasks without the close fence and the
revocations would let detached work commit after its consumer is gone, and ending the loops
without the fence would drop the last owner of a live session unrevoked. P2 does change
behaviour on its own, deliberately: a displaced or dropped session's refresh token is revoked,
background loops end with their client, and a cancelled caller no longer interrupts a refresh
or login; all covered by P2's tests. P3 is the user-visible part; the GTK menu item is the Linux
client's, on the same core call, and nothing breaks for it before then.

## Review log
**Round 1 — Codex + Vibe (Heavy).** Vibe: none. Codex, all accepted: whole minting operations
(refresh and login) spawned with their lock acquired inside; the displaced session revoked on
removal whatever the outcome; drop closes the store, fences late commits and revokes; every
loop wait wakes on shutdown, exits observed by join handle; one ordered event stream on the
Mac; a login gate and named tests/mutations for login cancellation and drop and for an ignored
observer; the halfway analysis corrected (P2 is one unit, and it changes behaviour on its own).
**Round 2 — Codex + Vibe.** Vibe: none. Codex, three new points, all accepted (no round 3,
nothing disputed): login reserves its generation before waiting and logout invalidates it; all
detached work spawns through a retained runtime handle (a drop on a Swift thread must not panic);
shutdown tests assert loop exit before releasing a gated minting response, then revocation after.
The gate closes.
