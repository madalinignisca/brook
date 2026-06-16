# gateway — reverse proxy & TLS termination (Caddy)

Single public entry point. **Owns all TLS** so individual services speak plaintext internally.

## Responsibilities
- Terminate **TLS** for HTTPS (REST) and **WSS** (WebSocket) — automatic certs via Let's Encrypt/ACME.
- Route: `/api/*` and `/ws` → `api`; serve/route MinIO and (optionally) Janus endpoints over HTTPS.
- One place for HSTS, security headers, and rate-limit/abuse rules at the edge.

## Why Caddy
Automatic HTTPS with near-zero config; simple `Caddyfile`. nginx/Traefik are drop-in alternatives if preferred.

## This directory will hold
`Caddyfile` (dev + prod variants) and TLS/routing notes. See [../../docs/SECURITY.md](../../docs/SECURITY.md).
