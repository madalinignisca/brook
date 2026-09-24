# Brook — Administrator Guide

How to self-host and operate a Brook server. For the developer/architecture
spec see the other files in `docs/`; for end users see the
[User Guide](user-guide.md).

> **Status: Phase 0.** What works today: a running server with **user accounts
> and login**. Chat, file transfer, calls, and bots are **planned** (see
> [ROADMAP.md](ROADMAP.md)) and this guide grows with them.

## What Brook is

A self-hosted, **own-your-data** team-communication server. You run it; your data
stays on your infrastructure. Not a SaaS, not end-to-end encrypted (see
[SECURITY.md](SECURITY.md) for the trust model) — designed for a small business
that wants to own its chat.

## Requirements

- A Linux host you control — a VM or LXC container on your homelab/server is ideal.
- **Docker** + the **Compose plugin** (`docker compose`).
- A couple of GB of disk for the database and (later) file storage.

## Install & run

```bash
git clone https://github.com/madalinignisca/brook.git
cd brook/deploy

make init     # creates .env with random secrets (only if it doesn't exist)
make up       # builds and starts: postgres + api + caddy
make ps       # confirm services are healthy
```

By default the API listens on **loopback only**: `http://127.0.0.1:8080`, reachable
from the server itself. To serve your LAN, set `BROOK_HTTP_BIND` in `.env` to the
server's LAN address (e.g. `BROOK_HTTP_BIND=192.168.1.10`) and run `make up` again.
The port is `BROOK_HTTP_PORT`. Verify from a client machine:

```bash
curl http://<BROOK_HTTP_BIND>:8080/health      # {"status":"ok","version":"..."}
```

> **Don't set `BROOK_HTTP_BIND=0.0.0.0`.** Docker Compose then publishes the port on
> every interface **including IPv6**. On a host with a public IPv6 address, that puts
> the server on the internet even when the LAN is behind NAT. Bind one LAN address.

> **Upgrading from an older release:** a `.env` created before `BROOK_HTTP_BIND`
> existed doesn't have it, so the server falls back to loopback and **LAN clients can
> no longer connect**. Add `BROOK_HTTP_BIND=<LAN IP>` to `.env` and `make up`.

### Create the first administrator

The **first** account created becomes the global **admin**:

```bash
curl -X POST http://<host-ip>:8080/api/v1/auth/register \
  -H 'content-type: application/json' \
  -d '{"handle":"admin","display_name":"Admin","password":"choose-a-strong-password"}'
```

After that, registration is closed to anonymous users — only an admin may create
further accounts (a proper admin/user-management UI is **planned**). For now an
admin creates a user by sending the same `register` request with an
`Authorization: Bearer <admin-access-token>` header (get a token via
`POST /api/v1/auth/login`).

## Day-to-day operations

| Command (in `deploy/`) | What it does |
|---|---|
| `make up` | build + start the stack (data preserved) |
| `make down` | stop the stack, **keep** data |
| `make logs` / `make ps` | follow logs / list services |
| `make rebuild` | rebuild + restart the `api` after pulling new code (applies DB migrations) |
| `make migrate` | apply database migrations to the latest version (without a rebuild) |
| `make dbrev` | show the current database schema revision + history |
| `make reset` | **wipe all data** (Postgres + storage volumes) and start fresh |
| `make storage` | also start MinIO (Phase 2 — file storage) |
| `make media` | also start Janus (Phase 4 — calls) |

`make reset` is destructive — it deletes the database. Use it to start over;
not for routine restarts.

## Upgrades & database migrations

The database schema is managed by **Alembic migrations**, not recreated on the
fly — so upgrading to a newer Brook **preserves your existing data**. The `api`
container runs `alembic upgrade head` automatically every time it starts, so the
normal upgrade is just:

```bash
git pull                 # get the new version
make rebuild             # rebuild the api image; it migrates on startup, then serves
make dbrev               # (optional) confirm the schema is at the latest revision
```

Migrations only ever *add to* or *transform* your data in place — they never
wipe it. Still, **take a backup before upgrading** (see below); it's the safe
habit for any schema change. If a migration ever fails, the `api` container stops
before serving (so it won't run against a half-migrated database) and the failure
is in `make logs`.

## Backups

Your data lives in the `pgdata` Docker volume. Back up with a logical dump:

```bash
docker compose exec postgres pg_dump -U brook brook > brook-backup-$(date +%F).sql
```

Restore into a fresh stack by piping the dump back into `psql` (same user/db).
Object storage (when MinIO is enabled in Phase 2) lives in the `miniodata` volume.

## Networking & TLS

- Phase 0 serves **plain HTTP** (trusted-network assumption), on loopback unless
  `BROOK_HTTP_BIND` names a LAN address. Don't expose it to the internet as-is.
- Moving to **TLS** is a one-file change in `services/gateway/Caddyfile` — switch
  the `:80` block to a real domain (automatic Let's Encrypt) or `tls internal`
  for a self-signed LAN certificate. No compose change needed.
- The media server's admin API is never exposed publicly by design.

## Secrets

`make init` writes random secrets to `deploy/.env` (JWT signing key, database
password). Treat `.env` as sensitive; it is git-ignored. **Do not** hand-edit the
database password after first run — it must match the existing `pgdata` volume
(if you must rotate it, `make reset` for a clean slate, or change it in both
places).

## Troubleshooting

| Symptom | Likely cause / fix |
|---|---|
| `api` exits immediately, logs mention the signing key | `BROOK_JWT_SIGNING_KEY` is weak/default. `make init` sets a strong one; ensure `.env` has a ≥32-char value. |
| `api` can't connect to the database after editing `.env` | The DB password in `.env` no longer matches the `pgdata` volume. Restore the old password, or `make reset` to rebuild. |
| Port 8080 already in use | Set `BROOK_HTTP_PORT` in `.env` to a free port, then `make up`. |
| Client can't reach the server | `BROOK_HTTP_BIND` must be the server's LAN IP (the default `127.0.0.1` is local-only), and the host firewall must allow the port. |

## See also

- [User Guide](user-guide.md) · [Security model](SECURITY.md) · [Roadmap](ROADMAP.md)
