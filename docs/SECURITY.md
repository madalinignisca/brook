# Security

> Principle: **only the call media is encrypted for free. Everything else is our responsibility.** Never trust the client.
>
> **Scope & trust model.** Brook is **self-hosted, own-your-data** software for small business. The security goal is *transport security + strong authentication + the operator fully controlling their server and data* — **not** hiding data from the server operator. **E2EE is an explicit non-goal**: if two people want server-opaque privacy, that's what Signal et al. are for. This keeps the system simple and operable by a small-business admin.

## 1. Transport summary — what's automatic vs. ours

| Channel | Encrypted by default? | Owner | Requirement |
|---|---|---|---|
| Call media / screen share (RTP) | ✅ **Always** (SRTP+DTLS, mandated by WebRTC) | WebRTC stack | none for transport crypto |
| WebRTC data channels | ✅ DTLS/SCTP | WebRTC stack | — |
| Signaling (SDP/ICE) | ❌ (rides our WebSocket) | **us** | **WSS** |
| REST API | ❌ | **us** | **HTTPS** |
| Chat WebSocket | ❌ | **us** | **WSS** |
| File transfer (presigned) | ❌ unless HTTPS | us + MinIO | **HTTPS**, short-lived URLs |
| Webhooks (bot in/out) | ❌ | **us** | HTTPS + signing + SSRF guard |

**TLS termination:** one reverse proxy — **Caddy** — terminates TLS for HTTPS + WSS (auto Let's Encrypt / ACME), **minimum TLS 1.2; prefer 1.3**. **All public/external traffic is TLS — no plaintext leaves the host.** *Internal* service-to-service traffic (api↔Postgres/MinIO/Janus) runs on a **private, trusted network** and may be plaintext in a single-host deployment; operators spanning hosts should enable TLS-to-DB / internal TLS (or mTLS). This is a deployment choice, not an app requirement (see §4a).

## 2. Calls — encryption depth (important caveat)

SRTP/DTLS is **hop-by-hop (client ↔ SFU)**. The SFU terminates encryption, so **the server can technically access media** (same as Zoom/Meet historically). **This is the model — by design**, because the operator owns and trusts their own server. **E2EE is a non-goal** (see scope note above); we do not layer SFrame/insertable streams. The wire is always encrypted (SRTP/DTLS); confidentiality *from the operator* is out of scope.

## 3. Authentication & authorization

**Authentication methods** (operator picks per deployment; full design in [AUTH.md](AUTH.md)):
- **Local** — username/email + password (**Argon2id**) + **optional TOTP** 2FA (RFC 6238) with recovery codes.
- **OIDC** — central auth for companies (test target: **Keycloak**); Authorization Code + PKCE per RFC 8252, system browser, JIT provisioning.
- **LDAP** — **pure LDAP** (e.g. OpenLDAP) over LDAPS/StartTLS; not the commercial AD/Workspace services.

All methods converge on **one internal session**: `api` issues a short-lived **access token (JWT)** + refresh token; everything downstream consumes that uniformly.

- **Call authorization:** signaling is **`api`-proxied** (see [ARCHITECTURE.md](ARCHITECTURE.md) §Signaling model) — `api` owns Janus sessions and authorizes joins from the client's **Brook session**; the client does **not** present a token directly to Janus. Any Janus-side token auth is managed server-side by `api`. A client can never join a room it wasn't authorized for.
- **AuthZ on every operation:** membership/permission checked server-side on **every** REST call **and every WebSocket message**. The client UI hiding a button is never the enforcement point.
- Presence/typing/messages are only fanned out to authorized channel members.
- **Token storage (client):** tokens kept in the platform keystore by `core` (Secret Service/Keychain/Credential Manager/Keystore).

## 4. Files

- Upload: client asks `api` for an **S3 POST Policy** (not a bare presigned PUT) → the policy embeds a **`content-length-range`** so MinIO **rejects oversize uploads at the edge, mid-transfer** — a presigned *PUT* cannot enforce size and would let a client write unbounded bytes (disk-fill DoS) before any `/commit` check. Download: presigned **GET** (single object, short TTL).
- Presigned URLs are **bearer capabilities** — anyone holding one within its TTL can use it. Mitigations: **short TTL**, HTTPS only, scope to a single object+operation, and `api` authorizes the requester before minting.
- At rest: see §4a — encryption at rest is the **operator's infrastructure concern**, not an app feature.

## 4a. Encryption at rest is the operator's, and must never block the app

A competent admin self-hosting on a small cloud server will often enable **encrypted volumes/disks, encrypted PostgreSQL, SSE on object storage, TLS to the database**, etc. Brook must be **agnostic** to all of it:

- The app treats **storage and the database as opaque dependencies**. It requires **no** specific at-rest scheme and must **work unchanged** whether the disk/DB/bucket is encrypted or not.
- No app logic may *depend on* or be *blocked by* at-rest encryption (no assumptions about plaintext-on-disk, no custom KMS coupling). Connection details (incl. TLS-to-DB, SSE buckets) are pure **configuration**.
- This is the right separation: **the app secures the wire and authenticates users; the operator secures the metal.** It keeps "own your data, self-host" friction-free for the sysadmin.

## 5. Bots & webhooks (features 6 & 7)

**Both directions are signed, with two distinct secrets per bot:**
- **Inbound** (external → channel as bot): an **inbound secret** the operator gave the external system. `api` only needs to *verify* HMAC signatures, so it stores a **hash** of this secret (shown once at creation). Requests carry an HMAC over body + timestamp; stale timestamps rejected (replay protection).
- **Outbound** (`/botname …` → bot URL): `api` must *produce* a signature, so the **outbound signing secret must be retrievable** — stored encrypted (envelope encryption via the app's key / operator secret store), not plaintext. The receiving bot verifies with its copy.

**SSRF — the sharp edge.** Users register bot webhook **URLs**. A malicious URL could target `localhost`, `169.254.169.254` (cloud metadata), or internal IPs.
- **Allowlist scheme = HTTPS only.**
- **Block** loopback, link-local, private (RFC1918), and metadata ranges — resolve the hostname and check the *resolved IP* (defend against DNS rebinding: re-resolve/pin at request time).
- No following redirects to disallowed targets.
- Per-bot **rate limiting** and payload size caps.

## 6. Other baseline controls

- **Input validation** on all API inputs; size limits per §7.
- **Rate limiting** on auth, message send, file requests, webhook calls — concrete thresholds in §7.
- **Secrets management:** DB creds, MinIO keys, JWT signing key, Janus admin secret, bot signing secrets — via environment/secret store, never in the repo. `.env.example` documents names only.
- **Least privilege** between services (e.g. MinIO bucket policy, DB roles).
- **No telemetry.** Logs are operational only and avoid message content.

## 7. Concrete limits & lifetimes (single source of truth)

Starting values — tune with real data; the point is that nothing is left "short-lived (unspecified)".

**Token & session lifetimes**
- Access token (JWT): **15 min**. Refresh token: **7 days**, **rotated** on each use (old one revoked; reuse ⇒ revoke the family).
- **Janus session** (server-side, owned by `api`): created on join, torn down on leave/disconnect/idle (no client-presented token — see §3).
- **Presigned URL** (PUT/GET): **10 min**.
- OIDC Brook exchange code: **60 s**, single-use.
- TOTP: 30 s step, ±1 window tolerance; recovery codes single-use.

**Size caps**
- Message body: **16 KB**. Webhook payload (in/out): **64 KB**. File upload: **100 MB** default (operator-configurable).
- WS first-frame `auth` timeout: **5 s**.

**Rate limits** (per user/IP unless noted)
- Auth (login/totp/ldap/refresh): **5 / min** then backoff/lockout.
- Message send: **30 / 10 s**. File-URL requests: **20 / min**.
- Outbound webhook per bot: **10 / s**. Inbound webhook per bot: **20 / s**.

**Call lifecycle / cleanup**
- A `call` with no active participants for **5 min** is closed: SFU room torn down, `calls.ended_at` set. Guards against orphaned rooms when a client crashes mid-call.
- **Don't trust the WS for "left."** A silently-dropped client leaves a half-open socket, so `api` must treat **Janus media state** as authoritative: subscribe to the **Janus Event Handler** (participant leaving / ICE/DTLS torn down) to definitively mark participants gone, in addition to explicit `call.leave` and WS close.

## 8. Threat-model notes (living)

- **Trust boundary = the operator's server.** By design the operator (and a fully compromised server) can access messages, files, and media. This is accepted: it's self-hosted, own-your-data software, not a zero-trust/E2EE product. Operators must secure their infrastructure (§4a).
- Compromised SFU ⇒ media exposure. Inherent to an SFU; accepted within the trust model above.
- Compromised `api` ⇒ message/file/metadata exposure. Accepted; mitigated operationally (least privilege, hardening, §4a at-rest).
- Stolen presigned URL ⇒ single-object exposure for the TTL window.
- Stolen access token ⇒ bounded by short TTL + refresh rotation; support revocation.
- Out of scope: confidentiality *from the server operator* (use Signal/alternatives for that).
