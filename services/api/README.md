# api — application backend

**Python + FastAPI.** The control + realtime plane for everything except media bytes.

## Responsibilities
- **Auth** — verifies all methods and issues one internal session (JWT + refresh): **local** (Argon2id + optional TOTP), **OIDC** (RP; Keycloak test target), **pure LDAP** (LDAPS/StartTLS). Full design: [../../docs/AUTH.md](../../docs/AUTH.md).
- Channels/DMs, membership, permissions (enforced on **every** request and WS message).
- Messages: persist + WebSocket fan-out; paginated history.
- Files: mint **presigned** PUT/GET URLs for MinIO (bytes never proxy through here).
- Bots: registry, signing secrets, channel membership; inbound webhook ingest; `/botname` outbound dispatch (HMAC-signed, **SSRF-guarded**).
- Calls: **own Janus sessions/handles** and **proxy signaling** (api-proxied) between client WS and the Janus VideoRoom API; authorize joins from the Brook session; deliver ICE servers/TURN credentials. Clients present no token to Janus.

## Tech
- FastAPI (async), WebSocket endpoint for realtime.
- `GET /health` liveness/readiness endpoint (Janus exposes its own); used by compose/orchestration.
- PostgreSQL (see [../../docs/DATA_MODEL.md](../../docs/DATA_MODEL.md)).
- Talks to **Janus** over its HTTP/WS API ([../sfu/](../sfu/)) and **MinIO** over S3 API ([../storage/](../storage/)).
- Sits behind **Caddy** ([../gateway/](../gateway/)) which terminates TLS.

## Develop (local)
```bash
cd services/api
uv sync --extra dev                 # create .venv + install (locked)
BROOK_ALLOW_INSECURE_AUTH=1 uv run uvicorn app.main:app --reload   # dev only

# Quality gates (all run in CI — see .github/workflows/api.yml):
uv run ruff check . && uv run ruff format --check .
uv run mypy app
uv run coverage run -m pytest && uv run coverage report   # >=80% gate
uv run bandit -q -r app tests -s B101,B105,B106
uv run pip-audit --skip-editable
```
> Startup **refuses a weak/default `BROOK_JWT_SIGNING_KEY`** (forgeable tokens). In prod set a strong (≥32-char) key; for local dev set `BROOK_ALLOW_INSECURE_AUTH=1`. Tests run against SQLite by default; set `BROOK_TEST_DATABASE_URL` (Postgres) to run the suite against Postgres (CI does both). Implemented so far (Phase 0): `/health`, local `register`/`login`/`refresh`/`logout`/`me` with first-user→admin bootstrap.

## Contracts
- Wire protocol: [../../docs/PROTOCOL.md](../../docs/PROTOCOL.md)
- Security model: [../../docs/SECURITY.md](../../docs/SECURITY.md)

> Why Python here (vs Rust in `core`): leverages existing strength, fast to iterate, and the api is **not** on the bandwidth-heavy path (media/files bypass it), so its performance envelope is comfortable.
