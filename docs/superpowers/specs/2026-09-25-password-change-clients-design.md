# Password change: core and macOS (auth hardening, PR B, client side)

> Status: **approved** (Heavy, two rounds; §9 amendment approved, two rounds) · 2026-09-25 · Dial: **Heavy** (auth; owner confirmed)
> Server side (`services/api` endpoints) is the server session's, by the owner's routing; this
> spec covers core and the macOS app, and states what they need from the server.

## 1. Goal
A signed-in user can change their password from the Mac app; an admin can set a new password for
another user (onboarding, "I forgot mine"). Done means, observably:
- After a change, this device stays signed in (new token pair) and its realtime socket keeps
  working. Every other session of that user loses its refresh token at once, so it is signed out
  at its next refresh; its current access token stays valid until it expires (≤ 15 min).
- A wrong current password or a too-short new one is refused with a message saying which.
- An admin reset revokes the target's refresh tokens (same ≤ 15 min access-token boundary), and
  leaves the admin's own session untouched. An admin cannot use it on themselves.
- Core calls and the macOS change sheet land in this PR; the admin sheet lands with them, using the
  server's user list (confirmed).

## 2. Not doing
- Password reset by email (no mail infrastructure), password strength meters, breach checks.
- Changing the policy (min 8, as register) — the server owns it; the client mirrors it only to
  give an early message.
- TOTP (PR D, separate spec).
- GNOME UI (the Linux client's; core API names were sent to it).

## 3. Wire (confirmed by the server session, 2026-09-25)
| Call | Body | Success | Errors |
|---|---|---|---|
| `POST /api/v1/auth/password` (Bearer) | `{current_password, new_password}` | 200 TokenPair (as `/login`): **all** the user's refresh tokens revoked, this fresh pair issued | 403 `auth.invalid_credentials` (wrong current — deliberately not 401, so it is never mistaken for an expired token), 422 validation (8..256, as register), 429 `auth.rate_limited` + `Retry-After` |
| `POST /api/v1/users/{id}/password` (Bearer, admin) | `{admin_password, new_password}` (owner's decision: the admin re-enters their own password, so a stolen access token alone cannot take over accounts) | 204: the target's refresh tokens revoked | 403 `auth.invalid_credentials` (wrong admin password), 403 `authz.forbidden` (not an admin, **or an admin target**), 400 `invalid` (own id), 404 `not_found`, 422, 429 |
| `GET /api/v1/users[?handle=…]` (Bearer, admin) | — | `[{id, handle, display_name, global_role}]` by handle; `?handle` exact match | 403 `authz.forbidden`, 404 `not_found` |

Any of these can also answer 401 when the access token expired (core refreshes once and retries,
§4). The old refresh token is dead the moment the server commits; a response lost after that
leaves the client with revoked tokens (§4, "Limits").

Choice: "revoke all, return a fresh pair" rather than "revoke all but the caller's". It needs no
server notion of "the caller's session", and it rotates this device's refresh token too, so a
refresh token leaked before the change is dead after it (a leaked access token lives ≤ 15 min).

## 4. Core
- `BrookClient::change_password(current, new) -> Result<()>`,
  `BrookClient::admin_reset_password(user_id, new) -> Result<()>`,
  `BrookClient::list_users() -> Result<Vec<UserSummary>>`; FFI mirrors all three.
- **The race this must not lose:** the server revokes every refresh token, including the one core
  holds. A background refresh that sends the old token after the change is rejected, and core's
  rejection path clears the session if it still holds that token → the user is signed out by
  their own password change. So `change_password` holds the single-flight **refresh lock** from
  before the request until the new pair is committed; a refresh waiting on the lock then sees a
  new credential revision and sends nothing. **Login takes the same lock** (review): a login
  cannot install a pair that a password change in flight is about to revoke.
- **Bound to one session:** the epoch is read before taking the lock and checked after; if it
  changed (logout / another login), nothing is sent (`NotAuthenticated`). Every step below uses
  only that epoch's credentials.
- **Inside the lock** (a lock-held helper; never `Refresher::refresh`, which takes the lock and
  would deadlock): send with the current access token. On **401** only (never on the 403
  `auth.invalid_credentials`): run one lock-free `refresh_once`, and continue only if it
  *committed within the same epoch*; `Rejected` / `NoSession` / `Discarded` → `NotAuthenticated`
  (the existing rejection path already cleared the session), a transient error → returned as is.
  Retry the change **once**. On success, commit the new pair compare-and-set against the refresh
  token **used for the successful attempt** (after a retry that is the rotated one), which bumps
  the credential revision; the socket re-authenticates with the new access token by the existing
  mechanism (no waiting on the socket inside the lock). If the CAS finds the session changed, the
  pair is dropped and the call returns `NotAuthenticated`.
- **Cancellation:** the locked section runs as its own task, so a caller that drops the future
  (a closed sheet, a cancelled Swift task) cannot stop it between the server's commit and core's.
- **Bounded:** the whole locked section (request, the one refresh, the retry) is bounded at 30 s,
  so a stalled response can never hold the refresh lock (and with it refresh and login)
  indefinitely. Expiry is the documented ambiguous outcome (`Timeout`; see "Limits").
- **Secrets:** passwords are never logged, never in `Debug`/`Display` of any error (FastAPI's 422
  body echoes the submitted value: the password endpoints map errors to fixed messages and codes
  and never carry the response body), and not stored by core after the request. `admin_reset_password`
  refuses the caller's own id locally (`Api{invalid}`) before sending.
- **Limits (documented, not fixed):** a response lost after the server committed (network drop)
  leaves this device with revoked tokens; it is signed out at its next refresh and signs in with
  the new password. Recovering it needs server support; out of scope.

## 5. macOS
- Signed-in view: **Change Password…** (sheet): current, new, confirm. Client-side checks before
  sending: new ≥ 8 characters, confirm matches, new ≠ current. Server errors mapped to messages:
  wrong current password; too short / refused by the server's rules; too many attempts (rate
  limit); unreachable. Success: "Password changed. Your other devices will be signed out within
  15 minutes."
- Admin only (`globalRole == "admin"`): **Reset a User's Password…**: pick a user from the
  server's list (admins and oneself are left out: the server refuses admin targets), enter and
  confirm a new password, and re-enter one's own password.
- Secure text fields. All fields are cleared on success and when the sheet closes; after a refused
  attempt they stay so a typo can be fixed.

## 6. Tests
- Core, TestServer (real single-origin server in-process):
  - success commits the new pair, bumps the credential revision, and the socket re-authenticates
    with the new access token; a later command works;
  - **the race:** a background refresh through the production `Refresher::refresh`, gated to
    arrive (a) while the change holds the lock and (b) just before it — the user is never signed
    out, and the token that reaches the server is asserted in each case;
  - 401 → exactly one refresh, exactly two password requests, the final installed pair is the one
    from the retry and it works; repeated 401 → no loop; a refresh rejected in between →
    `NotAuthenticated`;
  - 403 wrong current → `Api{auth.invalid_credentials}`, **zero** refreshes, session unchanged;
  - epoch changed before or during (logout, other login) → nothing sent / pair dropped; a login
    started during a change waits for it (lock);
  - cancellation after the server committed → the pair is still committed;
  - 422 whose body echoes the password → neither `Display` nor `Debug` contains it;
  - admin: 204 → Ok and the admin's session unchanged; own id → refused locally, nothing sent;
    server 400/403/404 mapped; `list_users` parses and maps 403.
  - log-secrecy test extended to all three calls.
- Live (itest, once the server lands): two sessions of one account; change on A → B's refresh is
  rejected, A keeps working; admin reset of a test account → its refresh rejected.
- macOS: validation (length, mismatch, same as current), error → message mapping, field
  lifetime (cleared on success and close, kept after a refusal), admin item only for admins and
  the admin absent from the pick list.
- Mutations: no lock; CAS against the start token; no epoch check; refresh on 403; no local
  self-check; error body carried.

## 7. Where this fails
| Failure | Response |
|---|---|
| Server chooses "keep the caller's session" instead of a fresh pair | Core's commit path becomes "no token change"; the lock is still needed only if the server revokes the caller's token |
| No endpoint to list users for the admin sheet | Admin reset by handle lookup, or defer the admin UI (core call still lands) |
| Access token expiry mid-change | one refresh + retry under the lock (tested) |

## 8. Review log
**Round 1 — Codex + Vibe (Heavy).** Accepted: CAS against the token of the successful attempt
(both); bind the operation to its epoch and let login take the refresh lock; a lock-held refresh
helper with defined outcomes; cancellation-proof locked section; lost responses documented as a
limit; fixed error messages (422 echoes input); precise wording (≤ 15 min access tokens, admin
session untouched, admin UI scope); local self-reset refusal; 401 in the wire table; the test list
above. Rejected with reasons: zeroizing passwords (they pass through Swift strings, UniFFI
buffers and the HTTP body; wiping one Rust copy protects nothing, and login does not either);
ordering of auth-state publication in `SessionStore` (a real, pre-existing gap not widened by
this spec — filed as its own follow-up).
**Owner decision (2026-09-25):** the admin reset asks for the admin's own password; the server
also refuses admin targets. Recorded in §3 and §5.
**Round 2 — Codex + Vibe.** Vibe: none. Codex: bound the locked section (a stalled response
would hold the refresh lock and block login) and the success message still overstated the
sign-out; both accepted. Nothing disputed; the gate closes.

## 9. Amendment: sign out other devices at once (server PR #45)
The owner asked for a **"Sign out of other devices"** checkbox, checked by default. The server
adds `sign_out_other_devices` (default `true`) to `POST /auth/password`. When true, every access
token issued before the change is refused at once (REST 401, WebSocket `1008 session_revoked`) and
open sockets are closed; when false, other devices stay signed in. An admin reset always signs
the target out this way, so its sheet gets no checkbox.
- Core: `change_password(current, new, sign_out_other_devices)`, always sent explicitly (never
  left to the server default). Nothing else changes. This device's own socket is closed too; core
  maps it to AuthRejected, and its refresh waits on the lock the change holds, sees the new
  revision and reconnects with the new pair (a call resumes, `call.resume`).
- macOS: the checkbox under the password fields, back to checked whenever the sheet opens. Success
  text follows the choice: checked → "Password changed. Your other devices are signed out."; not
  checked → "Password changed. Your other devices stay signed in." The admin success text becomes
  "…is signed out everywhere." (no 15 minutes).
- Tests: the exact body carries the flag both ways; the model sends what the box says, starts
  checked, is re-checked on clear, and shows the matching text; FFI passes it through. Core, with
  a fake that refuses the old access tokens when the flag is set: this device's socket closed with
  `session_revoked` **before** the response (the change still in flight) and **after** the commit,
  and a REST call of this device with the old access token answered 401 during the change. Each
  asserts no sign-out, no refresh sent, and reconnect/retry with the new pair. (A call resumes over
  the new socket with the rotated token: `call_tests::resume_uses_the_rotated_token`.)
- Live (P4), both settings, on throwaway accounts: **checked** (and admin reset): the other
  device's old access token is refused on REST at once, and its already-open socket is closed with
  `session_revoked`, well inside the 15-minute access lifetime; this device keeps working.
  **Unchecked:** the other device's access token, open socket and refresh all keep working.
- Limit: an older server ignores the field, so it signs others out within 15 minutes whichever
  way the box is set, and the success text would be wrong both ways. There is no released server
  and no capability mechanism in the protocol yet; **this PR merges only after #45 is deployed to
  the test and production servers**. A capability or version check is a protocol-wide decision
  (it concerns every addition, not this one), raised with the server side.

**Amendment review, round 1 — Codex + Vibe (Heavy).** Accepted: tests for the socket closed
before and after the commit and for an old-token REST call (all three seen red with the
single-flight revision check or the change's lock removed); P4 asserts immediate revocation, and
that unchecked keeps the other device working (also Vibe's point). Rejected with reasons: a
capability check before showing the box (no released server; merge gated on #45's deployment;
versioning is a protocol-wide decision, not this checkbox's).
**Amendment review, round 2 — Codex + Vibe.** Both: none. Nothing disputed; the gate closes.
