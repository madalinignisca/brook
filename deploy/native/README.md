# Brook — native deployment (AlmaLinux 10, systemd, no containers)

For a small personal server: one VM runs PostgreSQL, the api, Janus and Caddy as
systemd services. The container stack in `deploy/` stays the dev/test path; both
run the same code and the same Janus commit.

| Service       | Unit            | Listens on                          |
|---------------|-----------------|-------------------------------------|
| Caddy (TLS)   | `caddy`         | TCP 80, 443 and UDP 443 — public    |
| api           | `brook-api`     | 127.0.0.1:8000 only                 |
| Janus API     | `brook-janus`   | 127.0.0.1:8188 only                 |
| Janus media   | `brook-janus`   | UDP 20000-20099 — public            |
| PostgreSQL 16 | `postgresql`    | Unix socket only (no TCP)           |

Secrets are generated on the host into `/etc/brook/{api,janus}.env` by
`install.sh` and never leave it. The api reaches Postgres with peer auth over the
socket, so there is no database password. No TURN: clients reach the public SFU
directly (mobile data works; restrictive corporate/guest Wi-Fi is out of scope).

## Prerequisites

- DNS A record for the domain pointing at the reserved public IP.
- Cloud firewall (OCI security list) allows TCP 22/80/443, UDP 443, UDP 20000-20099.
- Deploy only a **merged** commit.

## Install or upgrade

From a checkout on the operator machine:

```sh
rev=$(git rev-parse --short origin/main)
git archive --prefix="brook-$rev/" origin/main | ssh busuioc "tar -x -C /var/tmp"
ssh busuioc "cd /var/tmp/brook-$rev && sudo BROOK_DOMAIN=chat.madalin.me \
    BROOK_PUBLIC_IP=129.159.197.241 ./deploy/native/install.sh"
```

The first run builds Janus (a few minutes on 2 OCPU); later runs rebuild it only
when the pinned commit changes. `alembic upgrade head` runs on every api start.

## Go live (first install only)

The **first registered account becomes the global admin**, and registration is
open until then. Create it over loopback **before** Caddy makes the site public:

```sh
ssh busuioc 'curl -fsS -X POST http://127.0.0.1:8000/api/v1/auth/register \
    -H "Content-Type: application/json" \
    -d @-' <<<'{"handle":"<admin>","display_name":"<Name>","password":"<password>"}'
ssh busuioc 'sudo systemctl enable --now caddy'
```

Further accounts: log in as admin and `POST /api/v1/auth/register` with the
admin's bearer token (registration is admin-only once a user exists).

Then the firewall (optional second layer; the cloud security list already
filters). Read `firewall.sh` first — it has a 5-minute dead-man switch:

```sh
scp deploy/native/firewall.sh busuioc:/var/tmp/ && ssh busuioc sudo /var/tmp/firewall.sh
```

## Verify after every deploy

```sh
ssh busuioc 'sudo ss -Hltunp'        # 8000, 8188 on 127.0.0.1 only; no 5432 at all
curl -fsS https://chat.madalin.me/health
ssh busuioc 'journalctl -u brook-api -u brook-janus --since -10min -p warning'
git ls-remote ssh://a1git/srv/git/stilbag-magento.git   # the git server on the same VM still works
```

## Shared-host rules (busuioc is also the git server)

- Never touch `/srv/git`, the `git` user or its keys.
- SELinux: never `chcon`. Custom paths get `semanage fcontext` + `restorecon`
  (install.sh does this for `/opt/janus/bin` and the api venv).

## Logs and restarts

`journalctl -u brook-api -f`, `journalctl -u brook-janus -f`, `journalctl -u caddy -f`.
`systemctl restart brook-api` is safe at any time; clients reconnect and resume
calls (docs/PROTOCOL.md §3). Restarting `brook-janus` ends active calls
(`call.ended` reason `sfu_restart`).
