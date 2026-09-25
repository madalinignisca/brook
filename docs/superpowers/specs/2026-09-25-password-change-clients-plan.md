# Password change, client side — implementation plan

> Status: **approved** (Heavy; round 1 applied, round 2 skipped — see log) · 2026-09-25 · Implements the approved spec
> [2026-09-25-password-change-clients-design.md](2026-09-25-password-change-clients-design.md) (Heavy).
> Branch `feat/core-password-change` from main. Tests first, each seen failing under a named mutation.

### P1 — Test server that can lose the race (core `test_support`)
- `RefreshMode::Strict`: refresh tokens are tracked; an unknown or revoked one is answered 401
  `auth.invalid_token` (today's `Rotate` rotates anything, so no race could show).
- `POST /api/v1/auth/password`: modes `Ok` (revoke every refresh token of the user, issue a fresh
  pair), `WrongCurrent` (403 `auth.invalid_credentials`), `Expired` (401 once, then as configured),
  `Echo422` (422 whose body echoes the submitted passwords), and a **gate** that holds the response
  after the server-side commit (for the race and the cancellation tests). Records every request
  (access token used, count, **and the JSON body**, so tests assert the exact passwords sent).
- A **stall** mode for `/auth/refresh` and `/auth/login` (never answers), to prove the bounds.
- `POST /api/v1/users/{id}/password` (204 / 400 / 403 / 404) and `GET /api/v1/users[?handle=]`.
- **Check:** unit tests of the fake itself (strict refresh rejects a revoked token).

### P2 — Core
- **First:** the HTTP client gets a request timeout (30 s), so no request — refresh, login, the
  password calls — can hold the refresh lock forever. Test: a stalled refresh, then a login →
  the login completes within the bound (paused time); a stalled login, then a refresh → same.
- `login` takes the refresh lock around its request and install.
- `change_password`: epoch read → lock → epoch check → request → (401: one lock-free
  `refresh_once`, must commit in the same epoch) → one retry → CAS commit against the token used
  for the successful attempt. The locked section is a spawned task whose **own body** is bounded
  at 30 s (the timeout is inside the worker, so expiry releases the lock; an outer timeout on the
  join handle would not).
  Errors from these endpoints are mapped without the response body (fixed codes/messages).
- `admin_reset_password` (local self-id refusal), `list_users` (`UserSummary`); both use the same
  one-refresh-then-retry on 401 and the same body-free error mapping.
- **Check:** every core test of spec §6, including the race through the production
  `Refresher::refresh` in both orders, 401 counts (1 refresh, 2 requests), zero refreshes on 403,
  epoch change before/during, login waiting on the lock, cancellation after commit, the 30 s bound
  (paused time) **with a queued login proceeding after it and a late gate release not
  committing** (also after the caller cancelled), exact request bodies for both password calls,
  **401 → one refresh + retry and repeated 401 → no loop for each of the three endpoints**, 422
  echo absent from `Display`/`Debug` for both password calls, log secrecy; mutations: no lock,
  CAS on the start token, no epoch check, refresh on 403, no self-check, body carried, timeout on
  the join handle instead of inside the worker, current password sent as the new one, no 401
  retry on the admin calls, no client timeout.

### P3 — Bindings and macOS
- FFI: `change_password`, `admin_reset_password`, `list_users` (+ `FfiUserSummary`), error
  mapping as today's `LoginError`.
- App: a signed-in toolbar/menu item **Change Password…** (sheet) and, for admins, **Reset a User's
  Password…** (sheet with a user picker). A small view model per sheet holds validation, the
  error → message mapping and field lifetime; views stay thin.
- **Check:** view-model tests (validation, mapping, field lifetime, admin visibility, the admin not
  in the list); off-screen renders of both sheets; the app's existing suite still green.

### P4 — Acceptance (after the server PR lands on the test server)
- itest, on **throwaway accounts** created per run (registered with a random handle; never the
  shared `mac` account, whose credentials other suites depend on): two sessions of one account —
  change on A → B's refresh rejected, A keeps working and its socket re-authenticates, **login with
  the new password works and with the old one is refused**; admin reset of another throwaway
  account → its refresh rejected, its login works only with the new password. A run that stops
  midway leaves only throwaway accounts behind. Required by name in `itest.sh`.

## Where this fails
| Point | Failure | Response |
|---|---|---|
| P2 | login holding the refresh lock waits behind a stalled refresh (no HTTP timeout today) | the client-wide request timeout lands first, with stalled-request tests in both directions |
| P2 | spawned task outlives the client | it owns clones of `http`/`session` only (no client handle), bounded at 30 s |
| P4 | server PR not on the test server yet | P1–P3 land behind green unit tests; P4 runs when it is deployed, before the PR merges |

## If it stops halfway
P1 is test-only. P2 changes live behaviour (a client-wide request timeout; login taking the refresh
lock): it lands only with the timeout first and its stalled-request tests. P3 exposes UI that calls endpoints which must exist on
the server: **the PR does not merge before the server PR is deployed and P4 passes.**

## Review log
**Round 1 — Codex + Vibe (Heavy).** Vibe: none. Codex, all accepted: a client-wide request timeout
before login takes the lock (a stalled refresh would otherwise block login forever); the 30 s
bound inside the worker with lock-release and no-late-commit checks; 401 retry and echo-422 tests
for every endpoint; exact payload assertions and a live new/old-password login check; throwaway
accounts for the live run. Round 2 skipped: nothing disputed, every change adds a check.
