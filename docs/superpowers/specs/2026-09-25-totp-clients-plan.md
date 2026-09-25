# TOTP two-factor sign-in, client side — implementation plan

> Status: **draft for review** · 2026-09-25 · Implements the approved spec
> [2026-09-25-totp-clients-design.md](2026-09-25-totp-clients-design.md) (Heavy). Starts after the
> server spec #48 is merged; P4 needs its implementation on the test server. Tests first, each seen
> failing under a named mutation.

### P1 — Test server (core `test_support`)
- `login` honours `supports_totp` for a TOTP user (200 `{totp_required, totp_token, expires_in}`;
  403 `auth.totp_client_required` without the flag).
- `/auth/totp` with modes: ok, wrong code (403 `auth.invalid_code`, token kept), expired (403
  `auth.totp_expired`), recovery (+ `recovery_codes_left`); a **gate** after the server issues the
  pair (for the ownership races); every request recorded with its body.
- `enroll` / `activate` (pair + cutoff: the old refresh tokens of the user revoked, old access
  tokens refused) / `disable` / `recovery-codes` / admin `totp/reset`; `/me` with
  `totp_enabled`, `recovery_codes_left`; `/auth/logout` recorded.
- **Check:** unit tests of the fake (a pending token is refused as a bearer; activation revokes).

### P2 — Core
1. `LoginOutcome` and the challenge: `login` sends `supports_totp`, returns `TotpRequired` with
   an opaque challenge (id, token, expiry; `Debug` redacted). The session store gains the current
   challenge id, cleared by `login`, `logout`, `cancel_totp` and a success.
2. `complete_totp` / `complete_recovery`: lock → ownership check → request → ownership check →
   apply (install + consume, or end on `totp_expired`, or keep on `invalid_code`); superseded →
   `ChallengeSuperseded`, and a pair issued for it is revoked best-effort.
3. `totp_activate` on the change-password machinery (lock across the request, CAS commit, own
   bounded task); `totp_enroll`, `totp_disable`, `totp_regenerate_recovery_codes`,
   `admin_reset_totp` on `Ctx::send`. Body-free errors throughout.
- **Check:** every core test of spec §6; mutations: pair installed after the password step;
  ownership check skipped (before or after the request); a wrong code ending the challenge;
  activation without the lock; the pending token in `Debug`, a log line, `Authorization` or the WS
  auth frame; a 401 on a wrong code triggering a refresh.

### P3 — Bindings and macOS
- FFI: `LoginResult` gains `TotpRequired { challenge: Arc<FfiTotpChallenge> }` (an object, so the
  token never crosses as a string); the new calls; `FfiMe` fields.
- App: the code step in the sign-in view (autofill content type, paste normalisation, recovery
  toggle, Back → `cancel_totp`, submit disabled in flight); Account menu items from
  `totp_enabled`; enrolment sheet (password → local CoreImage QR + grouped key → code →
  recovery codes with Copy/Save and confirmation); Turn Off; New Recovery Codes; low-codes notice;
  admin reset sheet. One view model per sheet, views thin.
- **Check:** view-model tests from spec §6; off-screen renders; the existing suite green.

### P4 — Live (after #48's implementation is on the test server)
- itest on throwaway accounts: the test computes codes from the URI's secret (RFC 6238, SHA-1,
  30 s): enrol → activate (another device's refresh dies; this device keeps working) → wait for
  the next step → sign in with password + code → the same code refused → recovery-code sign-in →
  regenerate → disable; admin reset signs the target out. Required by name in `itest.sh`.
- Rate limits: the suite spends about 15 credential checks and waits out any 429, as the password
  suite does.

## Where this fails
| Point | Failure | Response |
|---|---|---|
| P2 | ownership check only before the request | the check runs again after it, under the lock; tested with a gated server |
| P3 | a UniFFI object for the challenge outlives the client | it holds the id and token only; a stale one returns `ChallengeSuperseded` |
| P4 | the next-code wait makes the live run slow | ≤ 30 s per wait, twice per run |

## If it stops halfway
P1 is test-only. P2 changes login (the `supports_totp` flag is ignored by older servers, and a
non-TOTP login is unchanged), so it can land alone behind its tests. P3 exposes UI for endpoints
that must exist: **the implementation PR merges only after #48's implementation is deployed and
P4 passes.**
