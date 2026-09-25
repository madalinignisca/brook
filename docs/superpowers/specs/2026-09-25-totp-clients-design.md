# TOTP two-factor sign-in: core and macOS (PR D, client side)

> Status: **draft** · 2026-09-25 · Dial: **Heavy** (auth) · Wire: the server spec
> [2026-09-25-totp-server-design.md](2026-09-25-totp-server-design.md) (#48, under review)
> The server side (endpoints, data model, and the requirements in
> [2026-09-22-app-secret-encryption-design.md](2026-09-22-app-secret-encryption-design.md) §7) is
> the server's spec. This one covers core and the macOS app. The wire below is the server spec's §2,
> which took in every client requirement.

## 1. Goal
A local-account user can turn on TOTP two-factor sign-in from the Mac app, and signs in with a
password plus a code (or a recovery code) from then on. Done means, observably:
- With TOTP on, the password alone never yields a session: sign-in asks for a 6-digit code, and
  a correct code (or an unused recovery code) signs in; a wrong one says so and asks again.
- Turning it on shows a QR code rendered on the device and a manual-entry key, activates only
  after a correct code, and then shows the recovery codes **once**.
- Turning it off needs the password and a current code.
- An admin can reset another member's 2FA (their key is lost); the target is signed out
  everywhere.
- No TOTP secret, code, recovery code or pending token appears in logs, error text or `Debug`.

## 2. Not doing
- TOTP for OIDC or LDAP users (their provider owns MFA; AUTH.md §4).
- Passkeys/WebAuthn, SMS, email codes, "remember this device".
- Storing the TOTP secret on the client (the phone's authenticator holds it; the Mac shows it
  once, at enrolment).
- The GNOME dialog (the Linux client's; it uses the same core calls).

## 3. Wire (server spec §2)
| Call | Body | Success | Errors the client handles |
|---|---|---|---|
| `POST /auth/login` | `{handle, password, supports_totp: true}` (always sent) | 200 TokenPair, **or** 200 `{totp_required: true, totp_token, expires_in: 300}` | 401 `auth.invalid_credentials`, 403 `auth.totp_client_required` (only a client that omits the flag), 429 |
| `POST /auth/totp` | `{totp_token, code}` or `{totp_token, recovery_code}` | 200 TokenPair (+ `recovery_codes_left` after a recovery code) | 403 `auth.invalid_code` (the token stays usable: a typo never sends the user back), 403 `auth.totp_expired`, 429; **never 401** |
| `POST /auth/totp/enroll` (access) | `{password}` | 200 `{otpauth_uri, expires_in: 600}`, once; a new enroll replaces the pending one (the old QR stops working) | 403 `auth.invalid_credentials`, 409 `conflict` (already on), 429 |
| `POST /auth/totp/activate` (access) | `{code}` | 200 `{recovery_codes: [..], access_token, refresh_token}`: **every other session signed out, this device's old tokens included** | 403 `auth.invalid_code`, 409 `auth.totp_enrollment_expired` (enroll again), 409 `conflict` (nothing pending) |
| `POST /auth/totp/disable` (access) | `{password, code}` (code may be a recovery code) | 204 | 403 `auth.invalid_credentials` / `auth.invalid_code`, 429 |
| `POST /auth/totp/recovery-codes` (access) | `{password, code}` | 200 `{recovery_codes}`: replaces all | as disable |
| `GET /auth/me` | — | gains `totp_enabled: bool`, `recovery_codes_left: int\|null` | — |
| `POST /users/{id}/totp/reset` (admin) | `{admin_password}` | 204; the target signed out everywhere | as the password reset |

Why the pending step is a success-shaped 200 and never a 401: core maps errors from the status
and code only and never reads an error body (a FastAPI 422 echoes passwords), so a token in an
error body would be dropped; and core treats every 401 as an expired access token and refreshes,
so a wrong code answered 401 would start a refresh instead of asking again.

## 4. Core
- `login(handle, password)` returns `LoginOutcome::{LoggedIn, TotpRequired{challenge}}`. The
  challenge holds the pending token privately (not `Debug`, not exposed over FFI as a string: an
  opaque object), with its expiry. Nothing is installed in the session store until the second
  step succeeds; `AuthState` stays `Authenticating` meanwhile, and a new `login` or `logout`
  discards the challenge.
- `complete_totp(challenge, code)` / `complete_recovery(challenge, recovery_code)`: take the
  refresh lock like login, install the pair, return the remaining recovery-code count if the
  server sent one. `auth.totp_expired` ends the challenge (the UI goes back to the password).
- **`totp_activate` runs exactly like `change_password`:** the server revokes this device's
  tokens too and returns a new pair, so the call holds the refresh lock from before the request
  until the pair is committed (CAS against the token used), in its own bounded task. Without it a
  background refresh with the pre-activation token is rejected and the device signs itself out
  right after turning 2FA on. The own-socket close (`session_revoked`, after the response) is
  covered by the same tests as the password change.
- Every login sends `supports_totp: true`.
- Account calls on the same `Ctx::send` as the password change (one refresh on 401, body-free
  errors): `totp_enroll(password) -> otpauth_uri`, `totp_activate(code) -> recovery_codes`,
  `totp_disable(password, code)`, `totp_regenerate_recovery_codes(password, code)` (always ten
  new codes; every old one dies),
  `admin_reset_totp(user_id, admin_password)` (own id refused locally).
- Secrecy: the otpauth URI and recovery codes are returned to the caller and never logged; the
  log-secrecy test covers every new call and the challenge's `Debug`.

## 5. macOS
- Sign-in: after the password, a code field (6 digits, one-time-code content type so the
  system can autofill from the Passwords app), "Use a recovery code instead", and Back. Paste of
  "123 456" is accepted (spaces stripped). A wrong code says "Wrong or already-used code. Wait
  for the next one." (every code-accepting endpoint shares the replay guard, so the first sign-in
  right after activation needs the next code). Expired challenge → back to the password with a
  note. Recovery-code input is normalised (case, dashes, spaces); codes display as
  `iiii-xxxx-xxxx-xxxx-xxxx`.
- Account menu: **Turn On Two-Factor Sign-In…** / **Turn Off…**, from `totp_enabled`.
  Enrolment sheet: password → QR (rendered locally with CoreImage from the otpauth URI; never
  fetched) plus the key in groups of 4 for manual entry → code (`auth.totp_enrollment_expired` → a fresh enroll and a new QR) → recovery codes with
  Copy and Save…; the note "Your other devices are now signed out" (always true on activation), and a "I've saved these" confirmation before Done. The URI, key and codes are cleared
  from the model when the sheet closes.
- Low recovery codes (≤ 2 left, from `/auth/totp`'s count or `/me`'s `recovery_codes_left`): a notice after sign-in, offering **New
  Recovery Codes…** (password + code → the new set, shown once, as at enrolment).
- Admin: "Reset a User's Two-Factor Sign-In…", shaped like the password reset sheet.

## 6. Tests
- Core, TestServer: login sends `supports_totp`; login → TotpRequired and **nothing installed**;
  activate under the lock: a background refresh racing it never signs the user out, the pair is
  committed, and a close of this device's socket before or after the commit reconnects with it; complete with a correct code
  installs the pair; a wrong code keeps the challenge and never refreshes; expired → challenge
  ended; recovery path returns the count; a login or logout during the challenge discards it;
  a pending token never reaches `Authorization` or the WS auth frame; enrol/activate/disable/admin
  reset bodies exact, 401 → one refresh, errors body-free; secrecy of URI, codes and token in
  `Display`/`Debug`/logs.
- macOS models: code field validation and paste; state machine password → code → signed in /
  back; enrolment steps and field lifetime; menu item from `totp_enabled`; admin visibility.
- Live (itest, throwaway accounts): enrol, activate with a code computed in the test from the
  URI's secret (RFC 6238 SHA-1, 30 s), sign in with password + code, replay of the same code
  refused, sign in with a recovery code, disable; admin reset signs the target out.
- Mutations: token installed before the second step; 401 on a wrong code triggering a refresh;
  challenge kept after `totp_expired`; URI or codes in a log line.

## 7. Where this fails
| Failure | Response |
|---|---|
| The server answers the pending step with an error status | core cannot read its body (by design); the wire table above is the precondition |
| Clock skew on the phone | the server's ±1 step; the UI says "check your phone's time" after a wrong code |
| User loses the phone and the recovery codes | admin reset (§3); no self-service path by design |
| Autofill of codes on macOS | best effort (`oneTimeCode` content type); typing always works |

## 8. Open for the owner
Those of the server spec §8 (event retention; IPs in auth events). Turning TOTP on always signs
out the other devices (decided in the server review), so the enrolment sheet has no checkbox.
