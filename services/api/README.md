# api — application backend

**Python + FastAPI.** The control + realtime plane for everything except media bytes.

## Responsibilities
- **Auth** — verifies all methods and issues one internal session (JWT + refresh): **local** (Argon2id + optional TOTP), **OIDC** (RP; Keycloak test target), **pure LDAP** (LDAPS/StartTLS). Full design: [../../docs/AUTH.md](../../docs/AUTH.md).
- Channels/DMs, membership, permissions (enforced on **every** request and WS message).
- Messages: persist + WebSocket fan-out; paginated history.
- Files: mint **presigned** PUT/GET URLs for MinIO (bytes never proxy through here).
- Bots: registry, signing secrets, channel membership; inbound webhook ingest; `/botname` outbound dispatch (HMAC-signed, **SSRF-guarded**).
- Calls: mint **short-lived, room-scoped** Janus join tokens; relay signaling between client WS and the Janus VideoRoom API.

## Tech
- FastAPI (async), WebSocket endpoint for realtime.
- PostgreSQL (see [../../docs/DATA_MODEL.md](../../docs/DATA_MODEL.md)).
- Talks to **Janus** over its HTTP/WS API ([../sfu/](../sfu/)) and **MinIO** over S3 API ([../storage/](../storage/)).
- Sits behind **Caddy** ([../gateway/](../gateway/)) which terminates TLS.

## Contracts
- Wire protocol: [../../docs/PROTOCOL.md](../../docs/PROTOCOL.md)
- Security model: [../../docs/SECURITY.md](../../docs/SECURITY.md)

> Why Python here (vs Rust in `core`): leverages existing strength, fast to iterate, and the api is **not** on the bandwidth-heavy path (media/files bypass it), so its performance envelope is comfortable.
