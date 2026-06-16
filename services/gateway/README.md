# gateway — reverse proxy & TLS termination (Caddy)

Single public entry point. **Owns all TLS** so individual services speak plaintext internally.

## Responsibilities
- Terminate **TLS** for HTTPS (REST) and **WSS** (WebSocket) — automatic certs via Let's Encrypt/ACME (min TLS 1.2, prefer 1.3).
- Route: `/api/*` and `/ws` → `api`; serve presigned MinIO access over HTTPS.
- **Never route the Janus Admin API publicly** — it stays internal-only. Client call signaling is `api`-proxied (clients don't reach Janus's API), so the gateway exposes no Janus endpoint. See [../../docs/ARCHITECTURE.md](../../docs/ARCHITECTURE.md) §Signaling model.
- One place for HSTS, security headers, and rate-limit/abuse rules at the edge.

## Why Caddy
Automatic HTTPS with near-zero config; simple `Caddyfile`. nginx/Traefik are drop-in alternatives if preferred.

## This directory will hold
`Caddyfile` (dev + prod variants) and TLS/routing notes. See [../../docs/SECURITY.md](../../docs/SECURITY.md).
