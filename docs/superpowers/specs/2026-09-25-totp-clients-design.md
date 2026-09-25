# TOTP two-factor sign-in: core and macOS (PR D, client side)

> Status: **draft**, waiting on the server spec for the wire · 2026-09-25 · Dial: **Heavy** (auth)
> The server side (endpoints, data model, and the requirements in
> [2026-09-22-app-secret-encryption-design.md](2026-09-22-app-secret-encryption-design.md) §7) is
> the server's spec. This one covers core and the macOS app, and states what they need from the
> wire. Points marked **(wire)** are proposals until the server spec confirms them.

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

## 3. Wire (proposed to the server; confirmed by its spec)
| Call | Body | Success | Errors the client handles |
|---|---|---|---|
| `POST /auth/login` | `{handle, password}` | 200 TokenPair, **or** 200 `{totp_required: true, totp_token, expires_in}` **(wire)** | 401 `auth.invalid_credentials`, 429 |
| `POST /auth/totp` | `{totp_token, code}` or `{totp_token, recovery_code}` | 200 TokenPair (+ `recovery_codes_left` after a recovery code) **(wire)** | 403 `auth.invalid_code`, 403 `auth.totp_expired`, 429 — **never 401 (wire)** |
| `POST /auth/totp/enroll` (access) | `{password}` | 200 `{otpauth_uri}`, once | 403 `auth.invalid_credentials`, 409 `conflict` (already on), 429 |
| `POST /auth/totp/activate` (access) | `{code}` | 200 `{recovery_codes: [..]}`, once | 403 `auth.invalid_code`, 410/404 when the pending enrolment expired |
| `POST /auth/totp/disable` (access) | `{password, code}` | 204 | 403, 429 |
| `GET /auth/me` | — | gains `totp_enabled: bool` **(wire)** | — |
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
- Account calls on the same `Ctx::send` as the password change (one refresh on 401, body-free
  errors): `totp_enroll(password) -> otpauth_uri`, `totp_activate(code) -> recovery_codes`,
  `totp_disable(password, code)`, `admin_reset_totp(user_id, admin_password)` (own id refused
  locally).
- Secrecy: the otpauth URI and recovery codes are returned to the caller and never logged; the
  log-secrecy test covers every new call and the challenge's `Debug`.

## 5. macOS
- Sign-in: after the password, a code field (6 digits, one-time-code content type so the
  system can autofill from the Passwords app), "Use a recovery code instead", and Back. Paste of
  "123 456" is accepted (spaces stripped). Expired challenge → back to the password with a note.
- Account menu: **Turn On Two-Factor Sign-In…** / **Turn Off…**, from `totp_enabled`.
  Enrolment sheet: password → QR (rendered locally with CoreImage from the otpauth URI; never
  fetched) plus the key in groups of 4 for manual entry → code → recovery codes with Copy and
  Save…, and a "I've saved these" confirmation before Done. The URI, key and codes are cleared
  from the model when the sheet closes.
- Low recovery codes (≤ 2 left after using one): a notice after sign-in.
- Admin: "Reset a User's Two-Factor Sign-In…", shaped like the password reset sheet.

## 6. Tests
- Core, TestServer: login → TotpRequired and **nothing installed**; complete with a correct code
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
None yet; the server spec may raise some (recovery-code count, whether admins may have TOTP
reset by another admin).
