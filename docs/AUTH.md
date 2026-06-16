# Authentication

> smartChat targets **self-hosted small business**. Auth must cover a lone team with local accounts *and* a company with central identity — without forcing either on the other. The operator chooses which methods are enabled per deployment.

## Design principle: pluggable methods, one internal session

No matter how a user authenticates, the `api` issues **its own** short-lived **access token (JWT)** + refresh token. Everything downstream (REST, WebSocket, call authorization) consumes that single internal session — so the rest of the system is identical regardless of auth method.

```
  local pw(+TOTP) ┐
  OIDC (Keycloak) ├──► api verifies ──► issues smartChat session (JWT + refresh) ──► WS / REST / SFU
  LDAP            ┘                         (uniform everywhere)
```

## 1. Local accounts — password (+ optional TOTP)

The default, always available (a small team needs nothing else).

- **Password:** hashed with **Argon2id**. Never stored or logged in clear.
- **TOTP (optional 2FA):** RFC 6238, per-user opt-in. Enrolment via QR (otpauth URI); verify a code to activate. Issue **one-time recovery codes** (hashed) at enrolment.
- Login: password → if TOTP enabled, require a valid code → issue session.
- Brute-force defense: rate-limit + lockout/backoff on the login and TOTP endpoints.

## 2. OIDC — central authentication (test target: Keycloak)

For companies with an identity provider. smartChat is the **Relying Party (RP)**.

- **Flow:** OAuth 2.0 **Authorization Code + PKCE**, per **RFC 8252 (OAuth for Native Apps)** — use the **system browser**, never an embedded webview.
- **api-mediated (recommended):** the `api` is a *confidential* RP (holds the OIDC client secret server-side). The native client:
  1. opens the system browser to `api`'s `/auth/oidc/start` (with PKCE challenge),
  2. user authenticates at the provider (Keycloak),
  3. provider → `api` redirect (`/auth/oidc/callback`); `api` exchanges the code, validates the ID token,
  4. `api` redirects back to the app via **loopback (`127.0.0.1`)** or a **custom URI scheme** with a short-lived smartChat code,
  5. app exchanges that code (PKCE verifier) for the smartChat session (`POST /auth/oidc/exchange`).
- **Per-platform redirect target:**
  - **Desktop** (GNOME/macOS/Windows): **loopback** `http://127.0.0.1:<port>/` (RFC 8252).
  - **Mobile** (iOS/Android): loopback is not usable. **Prefer claimed HTTPS redirects — iOS Universal Links / Android App Links** (cryptographically bound to the app, so another app can't hijack them), driven by `ASWebAuthenticationSession` (iOS) / Custom Tabs (Android). A **custom URI scheme** `smartchat://oidc-callback` is a documented **fallback only** (custom schemes can be claimed by a malicious app).
  - `core` abstracts this as a "redirect strategy" the native layer supplies.
- **Identity mapping:** OIDC `sub` (issuer + subject) is the stable key → linked to a smartChat user. **JIT provisioning**: create the user on first login from claims (`preferred_username`, `email`, `name`). MFA is the provider's responsibility (so TOTP above is not layered on OIDC users).
- **Config (per deployment):** issuer URL, client id/secret, scopes, claim→profile mapping.

## 3. LDAP — pure LDAP (not the commercial services)

Focus on **plain LDAP (e.g. OpenLDAP)**. Azure AD / Google Workspace expose LDAP-ish or OIDC, but they are **not** the target; standard LDAP is.

- **Transport:** **LDAPS** or **StartTLS** required (no plaintext bind).
- **Modes:**
  - *Simple bind as user* — bind with the user-supplied DN/credentials to authenticate; or
  - *Service-account search + bind* — bind a read-only service account, search for the user (`uid`/`mail` filter), then bind as the found DN to verify the password.
- **Attribute mapping:** `uid`/`cn`/`mail` → smartChat profile; **JIT provisioning** on first login.
- **Authorization (group → role):** optionally read the user's LDAP groups (`memberOf` / group search) and map them to the smartChat **global role** (`admin`/`member`, see [DATA_MODEL.md](DATA_MODEL.md)) via configurable rules (e.g. `cn=it-admins → admin`). Absent a mapping, federated users provision as `member`; channel membership/permissions are still enforced by smartChat (see [SECURITY.md](SECURITY.md) §3). Same mapping applies to OIDC via a groups/roles claim. Deprovisioning upstream → mark the user `deactivated`.
- **Config (per deployment):** server URL, base DN, bind DN/filter, attribute map, group→role rules, TLS settings.

## 4. Account model

- A user record may be **local** (has a password hash) or **federated** (linked to an OIDC `sub` or an LDAP DN). The link is stored in an `identities` table (see [DATA_MODEL.md](DATA_MODEL.md)).
- An operator can enable any combination of methods. A typical small business runs **local + TOTP**; a larger one points at **Keycloak (OIDC)** or **OpenLDAP**.
- **TOTP applies to local accounts only**; federated users inherit MFA from their provider.

## 5. Where it lives

- `api` owns all verification and session issuance (see [../services/api/README.md](../services/api/README.md)).
- `core` (client) handles the session lifecycle: storing tokens securely (platform keystore), refresh, and driving the OIDC system-browser flow. It does **not** implement password/LDAP/OIDC verification itself.
- Endpoints: see [PROTOCOL.md](PROTOCOL.md) §Auth.

## Non-goal

**No E2EE / no client-side identity crypto.** Auth proves *who you are to the server you trust and operate*. Confidentiality from the server itself is explicitly out of scope — see [SECURITY.md](SECURITY.md).
