# TOTP two-factor sign-in, client side — implementation plan

> Status: **approved** (Heavy, two rounds) · 2026-09-25 · Implements the approved spec
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
   an opaque challenge (id, token, `expires_at`; `Debug` redacted). The session store gains the
   current challenge id. **Invalidation never waits for the refresh lock:** `login`, `logout` and
   `cancel_totp` clear the id first (a plain store write), then do anything that needs the lock;
   so an in-flight completion, which rechecks ownership under the lock before applying, can never
   install after them. `cancel_totp(&challenge)` clears the id only if it is that challenge's
   (a stale Back never cancels a newer one); it is idempotent and returns nothing.
   **Publishing is atomic:** the final ownership check and the install-and-consume are one
   session-store write (`install_if_current(challenge_id, pair)`), so a Back between a check and
   an install cannot exist. **Login has an attempt generation:** `login` bumps it before sending
   the password; its response publishes either outcome (a pair, or a new challenge) only if the
   generation is still its own, checked in the same store write, so a password response arriving
   after a logout or a newer login publishes nothing.
2. `complete_totp` / `complete_recovery`: lock → ownership check → request → ownership check →
   apply (install + consume, or end on `totp_expired`, or keep on `invalid_code`); superseded →
   `ChallengeSuperseded`, and a pair issued for it is revoked best-effort.
3. `totp_activate` on the change-password machinery (lock across the request, CAS commit, own
   bounded task); `totp_enroll`, `totp_disable`, `totp_regenerate_recovery_codes`,
   `admin_reset_totp` on `Ctx::send` (own id refused locally, nothing sent; the id is read
   from the same snapshot whose epoch `Ctx::send` is bound to, as `admin_reset_password` does,
   so an account switch in between makes the call fail rather than target the new identity). Body-free errors
   throughout. The public me type gains `totp_enabled` and `recovery_codes_left`.
- **Check:** every core test of spec §6, each seen red under its named mutation: pair installed
  after the password step; ownership check skipped before or after the request; invalidation
  waiting for the lock (logout during a gated completion → late sign-in); `cancel_totp` of a stale
  challenge clearing a newer one; a wrong code ending the challenge; the challenge kept after
  `auth.totp_expired`; activation without the lock; the pending token in `Debug`, a log line,
  `Authorization` or the WS auth frame; **the otpauth URI or a recovery code in a log line or an
  error's `Display`/`Debug`**; a 401 on a wrong code triggering a refresh; no local refusal of the
  own id in `admin_reset_totp`.

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
- itest on throwaway accounts; the test computes codes from the URI's secret (RFC 6238, SHA-1,
  30 s) and never reuses a step (every code-accepting endpoint shares the replay guard), waiting
  for the next step before each code-bearing call:
  1. Two devices signed in (A via the bindings, B raw). A enrols and activates: **both devices'
     old access and refresh tokens** are refused (raw probes), and the returned pair works (raw
     `/auth/me` with it).
  2. Next step: sign in with password + code → a pair. **Replay:** a *fresh* challenge (new
     password step) with that same code, still inside its ±1-step window → 403
     `auth.invalid_code`; then the next step's code on that challenge succeeds (so the refusal was
     the replay guard, not expiry or a consumed token).
  3. Recovery-code sign-in → `recovery_codes_left` = 9; the same recovery code on a fresh
     challenge → 403.
  4. Next step: regenerate (password + code) → 10 new codes; an old unused one is refused, and
     **a new one signs in** (fresh challenge).
  5. Admin reset **while TOTP is enabled**, with a second device's session open: its access token,
     refresh token and socket are refused/closed (raw probes); the password alone now signs in.
  6. A separate throwaway account: enrol, activate, and **disable** with the next step's code;
     then `/me` says `totp_enabled: false` and a **password-only login returns a working pair**.
  Waits: one per code-bearing call after the first, ≤ 30 s each (about 5 per run, ≤ 2.5 min).
  Required by name in `itest.sh`.
- Rate limits: the suite spends about 15 credential checks and waits out any 429, as the password
  suite does.

## Where this fails
| Point | Failure | Response |
|---|---|---|
| P2 | ownership check only before the request | the check runs again after it, under the lock; tested with a gated server |
| P3 | a UniFFI object for the challenge outlives the client | it holds the id and token only; a stale one returns `ChallengeSuperseded` |
| P4 | the next-code wait makes the live run slow | ≤ 30 s per wait, about five per run |

## If it stops halfway
P1 is test-only. **P2 cannot land alone:** it changes `login`'s return type, and with
`supports_totp` always sent a TOTP account would leave any client that ignores `TotpRequired`
stuck (GTK drives its UI from `AuthState` and would sit in Authenticating; KDE's
`client.login(..).await?` would stop compiling, and KDE is not in CI). So the core PR opens as a
**draft** and merges in one landing with the macOS step (P3), the GTK second step and a KDE
"not supported yet" shim, built by the Linux client on a branch from it. Older servers: a
server without TOTP ignores `supports_totp` (its `LoginIn` has no `extra="forbid"`; pydantic
ignores unknown fields), and a non-TOTP account's login is unchanged, as today's tests show.
**The landing waits for #48's implementation on the test server and a passing P4.**

## Review log
**Round 1 — Codex + Vibe (Heavy), plus the Linux client's review.** All accepted: invalidation
before the lock and ownership rechecked under it, a stale `cancel_totp` never clearing a newer
challenge; P2 cannot land alone (both reviewers and the Linux client), so a draft core PR and
one landing with every client; the server-compat claim backed by the schema; named mutations
for the kept-after-expiry challenge and for URI/recovery-code leaks; the own-id refusal of
`admin_reset_totp`; a P4 that proves replay on a fresh challenge, never reuses a step, probes
both devices' old tokens after activation, and resets an *enabled* target with its sessions
probed; the challenge's expiry, an idempotent `cancel_totp` and the me fields (Linux asks).
**Round 2 — Codex + Vibe.** Vibe: none. Codex raised four new points, all accepted (no round 3,
nothing disputed): the final ownership check and install are one atomic store write; login
gets an attempt generation checked when publishing either outcome; the own-id refusal is bound
to the epoch `Ctx::send` uses; P4 proves disable (state and a password-only login) and that a
regenerated recovery code works. The gate closes.
