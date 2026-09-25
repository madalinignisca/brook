# Sign out, and following a remote sign-out (core and macOS)

> Status: **approved** (Heavy, two rounds) · 2026-09-25 · Dial: **Heavy** (session lifecycle)
> A prerequisite of the TOTP client plan (its challenge is invalidated by `logout`), and a gap on
> its own: neither app can sign out, and the Mac does not notice being signed out remotely.

## 1. Goal
Done means, observably:
- **Sign Out** in the Mac's Account menu ends the session: the server revokes this device's
  refresh token, the realtime socket closes, and the app shows the sign-in screen.
- When core loses the session by itself (a refresh rejected after a password change elsewhere
  with "sign out other devices", an admin reset, a revoked session), the Mac shows the sign-in
  screen with "You're signed out. Sign in again." within one refresh attempt, instead of a
  window whose calls all fail. An open call ends.
- A pair that core discards (a refresh or login racing a sign-out or another login) is revoked
  on the server best-effort, so no live refresh token is left behind that no device holds.

## 2. Not doing
- Session restore across launches (the Mac stores no session today; unchanged).
- Wiping local data (there is none yet; the local-encryption spec #46 owns that).
- The GNOME menu item (the Linux client's; it uses the same core call).

## 3. Core
- `logout(&self)`: infallible from the caller's view.
  1. Take the session out **at once** (`replace(None)`, a new epoch), without waiting for the
     refresh lock, and publish `AuthState::LoggedOut`. The realtime task sees the epoch change
     and closes its socket; calls fail with `NotAuthenticated`; a refresh in flight commits
     nothing (its compare-and-set against the old token fails).
  2. Then, best-effort and bounded (the client's request timeout), `POST /auth/logout
     {refresh_token}` with the token it took out. Errors are logged by kind only and ignored:
     the local sign-out already happened.
- **Every refresh token core stops holding is revoked** (best-effort, one helper, one rule):
  - a pair **discarded** because the session changed meanwhile, from any path: a refresh's
    `commit_refresh`, a login's install, a password change's commit (its compare-and-set already
    refuses another epoch), and later the TOTP completions;
  - a session **displaced** by a successful login (login already takes the old session out up
    front; its refresh token is now revoked too);
  - the session taken out by `logout`.
  So a refresh, login or password change racing `logout` leaves no live refresh token that no
  device holds: the server rotated or issued it, and core revokes what it will not keep.
- **Cleanup outlives the client:** each revoke runs as its own task on the process-wide runtime
  (the bindings' `OnceLock` runtime), owning clones of the HTTP client, base URL and token,
  bounded by the request timeout. Dropping the `BrookClient` (the Mac drops it on sign-out)
  cancels none of them. **Every request that can mint a pair** (a refresh, a login, a password
  change, later the TOTP completions) likewise runs in its own task owning its clones, to
  completion within the bound, and applies the discard-and-revoke rule to its result itself; so
  a pair issued just before `logout` and delivered after the client is gone is still revoked.
- **Remote sign-out, as today:** a refresh answered 401 clears the session through
  `clear_if_holds`, which publishes `LoggedOut` (`session_store.rs`); this spec only makes the Mac
  follow it.
- No session: `logout` sends nothing and publishes `LoggedOut` (idempotent).

## 4. macOS
- Account menu: **Sign Out** (⇧⌘Q is taken by macOS; no shortcut). No confirmation dialog: it
  is reversible by signing in, and the server keeps everything.
- `SessionStore` subscribes to core's auth state (`AuthStateObserver`, already used in the
  binding tests). **Every sign-in attempt has an id** (a counter); the observer and the login
  completion carry the id of the attempt that created them, and anything arriving for an attempt
  that is no longer current is ignored. So a queued `LoggedOut` from a dropped client never signs
  out a newer one, and a stale login completion never restores a signed-in phase after Sign Out.
- **The first transition out of signed-in wins**, per attempt:
  - Sign Out: the phase becomes `.signedOut(error: nil)` and the attempt ends *before*
    `logout` is called, so a remote `LoggedOut` delivered afterwards is ignored (the user just
    chose to sign out; no message is lost that they need).
  - A remote `LoggedOut` handled first: `.signedOut(error: signedOut message)`; the attempt ends,
    and the Sign Out item is gone with the signed-in window.
  - **A `LoggedOut` counts only after core's `LoggedIn` for that attempt** (the observer tracks
    the sequence): a fresh client's initial `LoggedOut` is not a sign-out. A terminal `LoggedOut`
    that arrives before the Mac has handled its own successful login completion ends the attempt
    with the message, and that late completion is then ignored.
  In both, the client is dropped, an open call window closes (the call model's existing end
  path), and sheets close.
- Signing in again after either path starts from a fresh client, as today.

## 5. Tests
- Core: logout revokes the held refresh token on the server and publishes `LoggedOut`; the socket
  closes; a later call is `NotAuthenticated`; logout with no session sends nothing; logout with
  the server stalled returns within the bound and is still signed out locally. **No orphan, each
  ending with exactly the winner's refresh token live on the fake (or none):** a refresh held at
  the gate while `logout` runs; a password change held at the gate while `logout` runs (its
  response delivered afterwards); a login racing `logout`; a login displacing a session; and
  `logout` followed at once by dropping the client (the revoke still arrives); and **a pair
  issued while `logout` runs, the client dropped, the response delivered afterwards**, for a
  refresh and for a password change: the issued refresh token is revoked. A remote sign-out: a
  refresh answered 401 publishes `LoggedOut`.
- macOS: the model's phase follows a core `LoggedOut` (remote) with the message and Sign Out
  without it; both delivery orders of a remote `LoggedOut` and a Sign Out; a `LoggedOut` from a
  previous attempt's client after a new sign-in is ignored; a stale login completion after Sign
  Out is ignored; a fresh client's initial `LoggedOut` is not a sign-out; `LoggedIn` then
  `LoggedOut` delivered before the login completion ends signed out, with the late completion
  ignored; the call window's end path runs; the menu item exists only when signed in.
- Mutations: logout waiting for the refresh lock (a stalled refresh then blocks sign-out); no
  revoke of a discarded pair; the observer ignored.
- Live: sign out on the test server → the old refresh token is refused (raw probe).

## 6. Where this fails
| Failure | Response |
|---|---|
| The server is unreachable at Sign Out | local sign-out still happens; the refresh token lives until its TTL (the server's) |
| The best-effort revoke of a discarded pair fails | the same TTL bound; logged by kind |
| An observer event and a user Sign Out race | the first transition wins (§4); both end signed out |

## 7. Review log
**Round 1 — Codex + Vibe (Heavy).** Accepted: revoke every refresh token core stops holding,
from every path (displaced by a login, discarded by a refresh, login or password change), with an
orphan test per race ending in exactly the winner's token live; cleanup tasks outlive the client;
sign-in attempt ids fencing the observer and login completions, and "first transition out of
signed-in wins"; remote sign-out stated (`clear_if_holds` publishes `LoggedOut`) and tested (Vibe).
Rejected with reasons: keeping the "you're signed out" message when a remote `LoggedOut` was
queued before the user's own Sign Out; the user has just chosen to sign out, so the message tells
them nothing, and the first-transition rule makes the outcome defined either way.
**Round 2 — Codex + Vibe.** Vibe: none. Codex, two new points, both accepted (no round 3,
nothing disputed): every request that can mint a pair runs to completion in its own task and
revokes a discarded result itself, with issue-logout-drop-deliver tests; a `LoggedOut` is
terminal only after core's `LoggedIn` for that attempt, and ends it even before the login
completion is handled. The gate closes.
