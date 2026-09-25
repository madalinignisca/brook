# TOTP (optional 2FA): server design

Status: draft for Heavy review. Owner of the server side: services/api. Clients (core,
macOS, GTK) write their own spec against this wire. Builds on:
- `2026-09-22-app-secret-encryption-design.md`: the keyring (merged in #43), and its **§7,
  the seven hard requirements** that TOTP must meet. Each is cited below by number.
- PROTOCOL.md §1.1 (sessions, sign-out everywhere, #45) and the rate limiter (#36).

## 1. Goals / non-goals

**Goals:** per-user opt-in TOTP (RFC 6238) for local accounts; one-time recovery codes;
admin reset for a user who lost their device; a host-side escape hatch for an admin who
locked themselves out.

**Non-goals:** TOTP for federated (OIDC/LDAP) users, whose MFA is the provider's
(AUTH.md); WebAuthn/passkeys (later); forcing 2FA on anyone (a later server policy).

## 2. Wire (all under `/api/v1`)

Client constraints this wire is shaped around:
- Core maps errors from status and `code` only, and never reads an error body (a 422
  echoes passwords). So anything the client must *use* comes in a success body.
- Core treats **every 401** on an authenticated call as "access token expired" and
  refreshes. So nothing here answers 401 for a wrong code or a bad pending token.

### 2.1 Login

`POST /auth/login {handle, password, supports_totp?}`, unchanged for users without
TOTP. With TOTP active and a correct password, and `supports_totp: true`:

```
200 {"totp_required": true, "totp_token": "<jwt>", "expires_in": 300}
```

Wrong password: unchanged (401 `auth.invalid_credentials`). The client tells the two 200
shapes apart by `totp_required`.

**Clients that predate TOTP** don't send `supports_totp`. For a TOTP user they get
`403 auth.totp_client_required` instead of a 200 they would misread as a login with no
tokens. Old cores map that to "the server refused", and the user updates the app. The
protocol has no capability negotiation otherwise, so this flag is the negotiation.

`POST /auth/totp {totp_token, code}` or `{totp_token, recovery_code}`. The token must
decode as `type == "totp_pending"`: an access or refresh token here is
`403 auth.totp_expired` (test #4), so a leaked access token can never become an
unbounded code-guessing credential.
- `200` TokenPair. With a recovery code, plus `recovery_codes_left: int`.
- `403 auth.invalid_code`: wrong code, a replayed code, a used or wrong recovery code,
  **or a decrypt failure of the stored secret** (§5).
- `403 auth.totp_expired`: the `totp_token` is bad, expired or already used, or the user's
  sessions were revoked after it was issued. The client restarts at the password.
- `429 auth.rate_limited` + `Retry-After`.

### 2.2 The pending token (§7.2)

A JWT signed with the same key as access tokens:
`{type: "totp_pending", sub, jti, iat, iat_ms, exp = iat + 300}`.
- **Refused everywhere else.** `deps.user_from_access_token` and the WebSocket already
  accept only `type == "access"`, and `/auth/password` goes through the former. A test
  asserts each (REST 401, WS `auth_failed`, `/auth/password` 401).
- **Single use, claimed atomically.** `/auth/totp` takes the user-row lock (`lock_user`)
  *before* checking the `jti`, and records it in the same critical section, before tokens
  are issued. Two concurrent requests carrying one pending token (say a recovery code and
  a `now+1` code) are serialised by the lock, and the second finds the `jti` used. The
  `jti` is recorded when `/auth/totp` *succeeds* and refused after.
  It is kept in process memory until `exp` (bounded; one worker, like the hub and the
  socket registry). Stated plainly: across a restart, or with a second worker, one
  password entry could complete two logins within 300 s, but only with two different
  valid codes (the DB-side `last_used_step` refuses a reused one). That bounds it to a
  duplicate session, never a bypass.
- A failed code does **not** burn the token, so a typo doesn't send the user back to the
  password. The rate limiter bounds the attempts instead.
- Dead if the user signed out everywhere after it was issued (`session_revoked(iat_ms)`),
  if TOTP was disabled or reset in between, or if the **password changed** after it was
  issued. That includes a change with the sign-out box unchecked: a new column
  `users.password_changed_at`, set by `/auth/password` and the admin reset, is compared
  with the token's `iat_ms`. A token minted by proving the old password dies with it.

### 2.3 Enrolment and management (full access session only, §7.5)

| Call | Body | Answer | Notes |
|---|---|---|---|
| `POST /auth/totp/enroll` | `{password}` | `200 {otpauth_uri, expires_in: 600}` | Returned **once**; no GET ever returns it. `409 conflict` if TOTP is already active. Replaces an unfinished pending enrolment, so a QR code scanned earlier stops working and the client must show the new one. |
| `POST /auth/totp/activate` | `{code}` | `200 {recovery_codes: [10 strings], access_token, refresh_token}` | Verifies against the pending secret; sets it active; **signs out every other session** (#45 cutoff). Otherwise a refresh token stolen before enrolment would keep the account open without ever meeting the new factor. The new pair keeps this device signed in (same ordering as `/auth/password`: sockets close after the response). The codes are shown once. `403 auth.invalid_code`; `409 auth.totp_enrollment_expired` (scan again: call enroll); `409 conflict` if nothing is pending. |
| `POST /auth/totp/disable` | `{password, code}` | `204` | `code` may be a recovery code. Removes the secret and all recovery codes. |
| `POST /auth/totp/recovery-codes` | `{password, code}` | `200 {recovery_codes}` | Always issues 10 fresh codes and invalidates every old one, used or not, including when none are left. |
| `GET /auth/me` | | `+ totp_enabled: bool, recovery_codes_left: int\|null` | So the app shows Enable or Disable, and warns when codes run low (null when TOTP is off). |

A wrong password on these is `403 auth.invalid_credentials` (the #39 rule: never 401).

`otpauth_uri` = `otpauth://totp/{label}?secret={base32}&issuer={issuer}&algorithm=SHA1&digits=6&period=30`,
where `label` = `{issuer}:{handle}`. The label as a whole and every query value are
percent-encoded independently. `issuer` comes from `BROOK_TOTP_ISSUER` (default `Brook`).

### 2.4 Admin reset (§7.6)

`POST /users/{id}/totp/reset {admin_password}` → `204`. It has the same shape as the
password reset (#39):
- the admin re-authenticates (wrong: `403 auth.invalid_credentials`, rate-limited);
- one user per call;
- never your own id (`400 invalid`), never another admin (`403 authz.forbidden`);
- removes the target's TOTP and recovery codes and signs them out everywhere
  (#45 cutoff);
- writes an `auth_events` row with the actor.

**Host escape hatch** (an admin who lost device and codes, or bulk work):
`uv run python -m app.cli totp-reset <handle>` on the server. Same effects, and the event
is recorded with `actor = NULL` and `via = "host_cli"`. The HTTP API has no bulk path.

## 3. Data model (one Alembic migration)

**`totp`**, one row per user:

| Column | Type | Notes |
|---|---|---|
| `id` | uuid PK | AAD `row_pk` for the secret. A new row per enrolment, so a re-enrolled secret never reuses an AAD. |
| `user_id` | uuid, unique | FK users, cascade |
| `secret` | text | `SecretBox.encrypt(base32 secret, purpose=TOTP_SECRET, row_pk=id)`. Never stored in plaintext. |
| `activated_at` | timestamptz, null | null = pending enrolment |
| `pending_expires_at` | timestamptz, null | |
| `last_used_step` | bigint, null | Replay guard (§7.3): a step ≤ this is refused |
| `created_at` | timestamptz | |

**`recovery_codes`**:

| Column | Type | Notes |
|---|---|---|
| `id` | uuid PK | |
| `user_id` | uuid | FK, cascade, indexed |
| `lookup` | text(4) | The code's public id prefix. Indexed, unique per user. It finds the one row to verify, so a guess costs one Argon2, not ten. |
| `code_hash` | text | **Argon2id** of the secret part, via the existing `PasswordHasher` (§7.1) |
| `used_at` | timestamptz, null | |

**`auth_events`**, append-only (§7.6):

| Column | Type |
|---|---|
| `id` | uuid |
| `user_id` | FK cascade |
| `actor_id` | FK users, set null; null = self or host |
| `kind` | text |
| `via` | text: `api` or `host_cli` |
| `created_at` | timestamptz |

Kinds: `totp_enrolled`, `totp_activated`, `totp_disabled`, `totp_reset`,
`recovery_code_used`, `recovery_codes_regenerated`, `totp_guessing`, `password_changed`,
`password_reset`.
**No IP addresses or user agents** (GDPR minimisation: nothing here needs them). Rows go
with the user (cascade). Retention beyond that is an owner decision, noted in §8.

## 4. Verification

- **Secret generation:** `secrets.token_bytes(20)`, base32 for the URI; HMAC-SHA1, 6
  digits, 30 s, per RFC 6238 defaults (§7.7).
- **In-house TOTP,** about 20 lines over `hmac`/`hashlib`, tested against the RFC 6238
  appendix B vectors.
- Accept steps `now−1 … now+1` (±30 s of clock skew). Compare with `hmac.compare_digest`.
  The accepted step must be `> last_used_step`, else `403 auth.invalid_code` (replay).
  `last_used_step` is written in the same transaction, under the user-row lock (#39's
  `lock_user`), so two concurrent submissions of one code can't both pass.
- **The replay guard covers every endpoint that accepts a code** (§7.3): `/auth/totp`,
  `activate`, `disable` and `recovery-codes` all check and advance `last_used_step`
  through one function. So a code accepted at `activate` (or typed into a phishing
  "confirm" page) can't then be used to log in. Consequence, and intended: the first
  login right after activation needs the **next** code (≤ 30 s).
- **Recovery codes:** 10 codes, each a 4-character public `lookup` id plus 16 secret
  characters, from a 32-symbol alphabet without look-alike characters: **80 bits** of
  secret, shown as `iiii-xxxx-xxxx-xxxx-xxxx`. They are a passwordless substitute for the
  second factor, so they need real offline resistance even behind Argon2id. Input is
  normalised (case, dashes, spaces), the row is found by `lookup` (an unknown id still
  spends one dummy Argon2 for timing), and it is used once: `used_at` is set under the
  user lock.
- **Key rotation:** after a successful verify, if `needs_rewrap(secret)`, re-encrypt with
  the primary key in the same transaction.

## 5. Failure behaviour (§5.5 and §5.8 of the encryption spec)

Any `DecryptError` on `totp.secret` (on every code-accepting endpoint) is an
authentication failure: `403 auth.invalid_code`, byte-identical to a wrong code. It is
logged at ERROR with user id, key id and reason, never the value, and caught at the call
site (never a 500). It must never fall through to "not enrolled".

**Detection (§5.8), implemented as that section specifies:** the decrypt-failure counter
labelled by reason, and the startup canary. A decrypt failure is **not** recorded as a
limiter failure. Counted as a wrong code, a key dropped one step early would show up as
per-handle brute-force noise and 429s for every enrolled user, the exact silent failure
§5.8 exists to surface. It is an operator fault, and it is reported as one.

That exemption must not become a free amplifier: **decrypt failures have their own
budget**, per user, of 5 per hour. Beyond it `/auth/totp` answers 429 without attempting
decryption, and logs one line per window rather than one per request.

**Deliberate deviation from the encryption spec's wording:** §5.5 says "401". This spec
uses 403 instead: core treats 401 as "refresh your access token", and the caller of
`/auth/totp` has none. The property §5.5 cares about is preserved: generic, fail-closed,
no distinguishing detail.

## 6. Rate limiting (§7.4)

Everything here goes through #36's limiter **before** any Argon2 or HMAC work, keyed
`(client ip, handle)`:
- `/auth/totp`: one failure per wrong code or recovery code; success resets. Per-handle
  slowdown applies, so a stolen password plus a spray of codes across IPs is paced per
  account (never locked).
- **A stricter per-handle budget for codes, with numbers.** After 10 wrong codes for a
  handle within 24 h, each further code attempt for that handle must be ≥ 15 min apart,
  from any IP. That's ≤ 96 guesses per day. At ≈ 3 valid codes per 10⁶ (±1 step), an
  attacker who already holds the password succeeds with ≈ 0.03 % per day, or ≈ 0.9 % per
  month of continuous attack. Crossing the threshold writes an `auth_events` row
  (`totp_guessing`) and logs a warning, so the owner sees it long before that.
- **What ends the budget:** a successful code or recovery code, a password change (the
  owner's remedy for "someone has my password"), and an admin TOTP reset all clear the
  handle's code-failure history.
- **It can't be turned against the owner.** An attacker holding the password could
  otherwise spend the budget and keep the real user waiting indefinitely. So the budget
  exempts IPs that completed a login for this handle recently (the limiter's existing
  trusted-IP set, 30-day TTL, #36). The owner's own devices and home network are never
  paced by an attacker elsewhere. From a new network the owner waits at most 15 min per
  attempt, or changes the password from a trusted device, which resets the budget.
- The password checks in `enroll`, `disable`, `recovery-codes` and the admin reset: as
  in #39.
- Login's password stage succeeding with TOTP pending records **no** success yet. The IP's
  streak resets only when the whole login completes.

## 7. Tests (each must be seen red under mutation)

1. RFC 6238 vectors.
2. Replay: the same code twice gives `403`, **across endpoints** (a code accepted at
   `activate` or `disable` is refused at `/auth/totp`).
3. Skew: ±1 step is accepted, ±2 is refused.
4. The pending token is refused on REST, WS and `/auth/password`; it is single-use; it is
   dead after a sign-out everywhere. **And the mirror:** an access token presented as
   `totp_token` gives `403 auth.totp_expired`.
5. Recovery codes are stored as Argon2id: a test asserts a stored hash verifies as Argon2
   and is **not** a hex SHA-256 digest (§7.1, the structural guard).
6. Recovery codes are single-use; `recovery_codes_left` counts down.
7. A decrypt failure gives `403 auth.invalid_code`, never 200, never 500, and never skips
   the code step (corrupt the stored value).
8. Enrolment: the URI is returned once; `409` when active; a pending enrolment expires.
9. Admin reset: not yourself, not an admin, re-auth required, target signed out, event
   recorded with the actor.
10. Rate limiting: code spraying reaches 429 before HMAC work.
11. Concurrent submission of one code: exactly one success (Postgres interleaving, like
    `test_token_races.py`).
12. App-level: `create_app()` refuses to start without a keyring (hardening left over from
    #43).
13. A decrypt failure increments the §5.8 counter and does **not** add a limiter failure.
14. The 24 h code budget: the 11th wrong code gets `Retry-After` ≥ 900, and a
    `totp_guessing` event is written.
15. A login without `supports_totp` for a TOTP user gives `403 auth.totp_client_required`.
16. Activation signs out other sessions: a refresh token from before activation gets 401,
    and the activating device's returned pair works.
17. A pending token dies on a password change with the box unchecked.
18. Two concurrent `/auth/totp` calls with one pending token and two valid inputs: exactly
    one TokenPair (Postgres).
19. Recovery codes: one Argon2 per guess (an unknown `lookup` spends a dummy), 80-bit
    secret part.
20. The code budget resets on success, on password change and on admin reset; a trusted IP
    is not paced.
21. Decrypt-failure budget: the 6th decrypt failure in an hour gives 429 with no decrypt
    attempted.

## 8. Open points for the owner

1. `auth_events` retention: keep as long as the account exists (proposed), or prune after
   N months?
3. Should `auth_events` record the client IP for `totp_reset`, `recovery_code_used` and
   `totp_guessing`? Useful forensically, and it is personal data under GDPR. Proposed:
   no by default; the limiter's escalation log line (journald, host retention) already
   carries the IP for abuse cases.
2. ~~Should enabling TOTP also sign out other devices?~~ **Decided: yes, always** (review):
   otherwise a token stolen before enrolment bypasses the new factor.
