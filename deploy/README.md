# deploy

Local development brings the whole server side up with one command; production runs the same components behind Caddy with real certificates.

## Local dev
```
docker compose up
```
Brings up:
- **postgres** — database
- **minio** — object storage (+ console)
- **janus** — SFU (VideoRoom)
- **caddy** — TLS / reverse proxy (single entry point)
- **api** — FastAPI backend

Copy `.env.example` → `.env` and fill secrets (DB creds, MinIO keys, JWT signing key, Janus admin secret). **Never commit `.env`.** See [../docs/SECURITY.md](../docs/SECURITY.md).

## Production notes
- Same components; Caddy obtains real certs (Let's Encrypt) for HTTPS + WSS.
- Add **coturn** for restrictive NATs (calls phase); see [../docs/MEDIA.md](../docs/MEDIA.md) §TURN.
- **Janus Admin API stays on the internal network — never published by Caddy.**
- `api` realtime layer is **stateful**: **MVP = single node**; scale-out uses **sticky WS + Redis/NATS pub-sub** with node-affine call signaling (see [../docs/ARCHITECTURE.md](../docs/ARCHITECTURE.md) §Realtime state & scaling). It is *not* a stateless horizontal tier.

> `docker-compose.yml` here is a **skeleton** to flesh out in Phase 0 ([../docs/ROADMAP.md](../docs/ROADMAP.md)).
