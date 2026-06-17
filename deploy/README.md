# deploy

The Brook server as one `docker compose` stack. Default is the lean **Phase 0
login stack**: `postgres` + `api` + `caddy`. Object storage (MinIO, Phase 2) and
the media SFU (Janus, Phase 4) sit behind compose **profiles** so they don't run
until you need them.

## Quick start (on your homelab VM / LXC)

Needs Docker + the Compose plugin. From this `deploy/` directory:

```bash
make init      # create .env with random secrets (JWT key, DB/MinIO passwords)
make up        # build + start postgres + api + caddy
make ps        # check it's healthy
make logs      # follow logs
```

The API is then reachable at **`http://<vm-ip>:8080`** (port set by
`BROOK_HTTP_PORT`). Smoke-test it:

```bash
curl http://<vm-ip>:8080/health
# {"status":"ok","version":"0.0.0"}

# create the FIRST user — it bootstraps as admin:
curl -X POST http://<vm-ip>:8080/api/v1/auth/register \
  -H 'content-type: application/json' \
  -d '{"handle":"you","display_name":"You","password":"a-good-password"}'
```

## Everyday ops

| Command | What it does |
|---|---|
| `make up` | build + start the core stack (data preserved) |
| `make down` | stop the stack, **keep** data |
| `make reset` | **wipe** all volumes (Postgres + MinIO) → fresh rebuild of the **core** stack (re-run `make storage`/`make media` if you were using them) |
| `make rebuild` | rebuild + restart just the `api` (after code changes) |
| `make logs` / `make ps` | follow logs / list services |
| `make storage` | also start **MinIO** (Phase 2) |
| `make media` | also start **Janus** (Phase 4) |

`make reset` is the clean-slate button: it removes the database and object data
so you can start over from an empty server.

## Connecting the GNOME client (Phase 0, plain HTTP)

The client enforces HTTPS by default. For this plain-HTTP LAN server, opt in with
`BROOK_ALLOW_INSECURE_HTTP=1` (dev only — credentials travel in cleartext):

```bash
cd ..   # repo root
BROOK_SERVER=http://<vm-ip>:8080 \
BROOK_ALLOW_INSECURE_HTTP=1 \
  cargo run -p brook-gnome
```

## Moving to TLS later

Flip `services/gateway/Caddyfile` from the `:80` block to either a real domain
(Let's Encrypt, incl. DNS-01 for a private host) or `tls internal` for a
self-signed LAN CA — no compose change needed. Then drop
`BROOK_ALLOW_INSECURE_HTTP` and point the client at the `https://` URL. See
[../docs/SECURITY.md](../docs/SECURITY.md).

> Secrets live in `.env` (git-ignored). Treat `make init`'s generated values as
> real secrets. The Janus Admin API is never published (internal only). The
> `api` realtime layer is stateful — single node for now (see
> [../docs/ARCHITECTURE.md](../docs/ARCHITECTURE.md) §Realtime state & scaling).
