# Admin adds users from the client (#265): core, Apple binding, Mac

Review dial: **Heavy** (two external reviewers plus a security review): permissions (an admin-only
endpoint), a new public API (`BrookClient::create_user`, the FFI method), two passwords in one form,
and a server contract change in flight (`admin_password` on register).

## What exists
- Server `POST /api/v1/auth/register` (`services/api/app/routers/auth.py`): the first user bootstraps
  open as admin; afterwards a caller that is not an admin (or sends no token) gets `403 authz.forbidden`;
  a taken handle is `409 conflict` (case-sensitive; disabled accounts keep their handle); new accounts
  are always `member`; per-IP limiter, `429 auth.rate_limited` + `Retry-After`. Limits: handle 2-64
  `^[A-Za-z0-9_.-]+$`; display name 1-64 after trimming (the schema says 128, `register` then enforces 64);
  password 8-256; a schema failure is `422 validation.error`, a refused display name `422 profile.invalid`.
- The reset routes (`routers/users.py`) re-authenticate the admin (`admin_password`); a wrong one is
  `403 auth.invalid_credentials`. The server agent adds the same to `register` (bootstrap unchanged).
- Core `account.rs`: `Ctx::send` (bearer, refresh once on 401), `account_error`, `admin_reset_password`,
  `list_users` are the patterns. Mac: `AdminResetModel` (re-asks the admin's password) is the closest pattern;
  the Account menu shows admin items when `user.globalRole == "admin"`.
- The Mac has no cached member list: New Message / Add Member take a typed handle the server resolves, and the
  admin reset sheets call `listUsers()` on open, so a new account is reachable at once.

## Done means
1. The Account menu shows **Add User…** only when `globalRole == "admin"` (a static predicate, tested).
2. Core `BrookClient::create_user(handle, display_name, password, admin_password) -> Result<UserSummary>`:
   `POST api/v1/auth/register` with bearer and `{handle, display_name, password, admin_password}`; returns the
   server's `id, handle, display_name, global_role`.
3. Error codes (stable, GTK relies on them): 409 `conflict`; 403 `authz.forbidden`; 403 `auth.invalid_credentials`;
   422 `validation` (message fixed, nothing from the body carried); 429 `auth.rate_limited`; 401 refreshes once and
   retries once, a second 401 or no session is `NotAuthenticated` and with no session nothing is sent; a 201 whose
   body does not parse is `UnexpectedResponse`.
4. Neither password appears in any log line at trace level or in any error's Display/Debug.
5. FFI `create_user(handle, display_name, password, admin_password) -> FfiUserSummary`, arguments in order (a test
   against TestServer proves each reaches the right JSON key).
6. The sheet: Handle, Display name, Password, Confirm, **Generate** (16 characters from `SystemRandomNumberGenerator`,
   56-character alphabet without look-alikes, shown once as selectable text; editing Password afterwards hides it and
   empties Confirm), **Your password** (the admin's own), Cancel, Add User.
7. Add User stays disabled, with the reason shown, until: the cleaned handle matches `^[A-Za-z0-9_.-]{2,64}$` (case as
   typed); the trimmed display name is 1-64 scalars; the password policy passes; your password is not empty.
8. Success: "`<handle>` was added. Give them the password; they can change it under Change Password." All secret
   strings (both passwords, Confirm, generated text) are emptied.
9. Failures, one line each, fields stay so a typo can be fixed: conflict ("handle taken; handles are case-sensitive and a
   disabled account keeps its handle"); conflict right after a no-answer try on the same handle ("your previous try got no
   answer and probably created it, with the password of that try"); `auth.invalid_credentials` ("Your own password is
   wrong."); `authz.forbidden` ("not allowed: your account may no longer be an admin, or your sign-in expired: try
   again"); `validation` (the limits); `auth.rate_limited` (the existing text); Network/Timeout/Disconnected/Unexpected
   ("no clear answer, the account may have been created; try again, and if it says the handle is taken, it was");
   NotAuthenticated (signed out text); anything else (unexpected).
10. A second submit while one is in flight is a no-op (guard and disabled button).
11. Cancel, closing, or the sheet disappearing on sign-out empties all secrets.
12. Not reachable while signed out (attached to `SignedInView`); a session that ends mid-call is `NotAuthenticated`.
13. Right after success the new handle opens a DM from New Message and is in the admin-reset user list, no restart.

## Not doing
Deactivate/promote/demote; invite links; forced password change at first sign-in; bulk import; updating `globalRole`
live (a demotion mid-session leaves the entry and the server refuses); hiding the entry offline; a Copy button or any
clipboard code (owner may ask; then `org.nspasteboard.ConcealedType` and clear after ~60 s); creating the first user of a new server (core sends nothing without a session; the open bootstrap is not this call); zeroing memory (Swift/Rust
copies are dropped, not wiped; "cleared" means the model and the field no longer hold it, as the existing password
calls); a local admin check in core (the server decides); showing `Retry-After`; GTK (its own change, same core call).

## Where it fails
- A stolen admin token creating accounts for up to 15 minutes: closed by `admin_password` once the server PR lands;
  until then the server ignores the extra field (nothing breaks, the gap stays).
- An expired/revoked token gets 403, not 401, on register (`_optional_user` returns None): core cannot tell it from a
  real refusal and does not refresh; the server should answer 401 for a presented-but-rejected token (server agent's call). **Gate: no client sheet (Mac or GTK) ships until register answers 401 for a presented-but-rejected token; if the server keeps 403, core adds one SingleFlight refresh-and-retry on `authz.forbidden`, never on `auth.invalid_credentials`.**
- Role changes mid-sheet: the server reads the role from the database, so a demotion is refused at once.
- A timeout after the server created the account: a retry says 409; the model remembers a no-answer try for that handle.
- A server 500 (or a bare gateway 5xx) can follow a committed insert: shown as 'no clear answer', and a retry's 409 says it was probably created (with the password of that try). The two-admins race is a 409 once the register PR lands.
- Rate limit shared per IP with login and refresh; five wrong admin passwords put the IP in backoff (as the reset routes).
- The generated password is visible until the sheet closes; if copied it goes to the pasteboard, which Brook does not clear.
- Autofill / password managers: no content types are set; third-party managers may offer to save; check by hand.
- Display name limit is 64 on the server (schema says 128): the client validates 64.

## Decisions taken (owner may override)
No Copy button; the "probably created it" wording after a timeout; the entry is not hidden offline.
