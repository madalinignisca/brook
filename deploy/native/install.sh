#!/bin/bash
# Brook native install/upgrade for AlmaLinux 10 (systemd, no containers).
#
# Run as root from the root of a Brook source tree (a `git archive` of a merged
# commit; see README.md). Idempotent: the first run installs, later runs upgrade
# in place. It never starts Caddy (the public entry point): on a fresh server the
# first registered account becomes admin, so the admin has to exist before the
# site is reachable. README.md "Go live" does that, then enables Caddy.
#
# It also never touches the firewall; firewall.sh does that on its own, with a
# dead-man switch, because a mistake there can lock out SSH.
#
# Required env:
#   BROOK_DOMAIN     public hostname, e.g. chat.madalin.me (DNS A record must exist)
#   BROOK_PUBLIC_IP  the address clients send media to (OCI reserved public IP)
#
# Shared-host rules (this VM is also a git server): nothing here touches /srv/git
# or the git user, and SELinux labels are only ever set via `semanage fcontext` +
# `restorecon`, never `chcon` — a chcon'd label is lost on the next policy rebuild.
set -euo pipefail

: "${BROOK_DOMAIN:?set BROOK_DOMAIN}"
: "${BROOK_PUBLIC_IP:?set BROOK_PUBLIC_IP}"
RTP_PORTS=20000-20099

# Keep in sync with services/sfu/Dockerfile: the same Janus runs in dev and prod.
JANUS_TAG=v1.4.2
JANUS_COMMIT=0a24110ae55a172c4293749b763dbb66a138f9ec

SRC=$(pwd)
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
[ -f "$SRC/services/api/pyproject.toml" ] || { echo "run from a Brook source tree" >&2; exit 1; }

log() { printf '\n==> %s\n' "$*"; }

# ---------------------------------------------------------------- packages
log "packages (CRB + EPEL)"
# EPEL carries libnice, libwebsockets, gengetopt, caddy and uv for EL10; CRB
# carries libsrtp/jansson/libconfig -devel. Approved for this host 2026-09-25.
dnf -y -q install epel-release
crb enable >/dev/null
dnf -y -q install \
    gcc make autoconf automake libtool pkgconf-pkg-config gengetopt git \
    libnice-devel libsrtp-devel libwebsockets-devel jansson-devel libconfig-devel \
    openssl-devel glib2-devel opus-devel libogg-devel zlib-devel \
    postgresql-server caddy uv rsync policycoreutils-python-utils

# ---------------------------------------------------------------- users
log "service users"
id brook >/dev/null 2>&1 || useradd --system --no-create-home --home-dir /opt/brook --shell /sbin/nologin brook
id janus >/dev/null 2>&1 || useradd --system --no-create-home --home-dir /opt/janus --shell /sbin/nologin janus

# ---------------------------------------------------------------- SELinux
log "SELinux file contexts"
# Executables under /opt get usr_t by default. Label them bin_t explicitly so
# systemd runs them as ordinary unconfined services regardless of how the policy
# treats usr_t. `-a` fails if the rule exists, so fall back to `-m`.
fcontext() { semanage fcontext -a -t "$1" "$2" 2>/dev/null || semanage fcontext -m -t "$1" "$2"; }
fcontext bin_t '/opt/janus/bin(/.*)?'
fcontext bin_t '/opt/brook/api/\.venv/bin(/.*)?'

# ---------------------------------------------------------------- Janus
if [ "$(cat /opt/janus/.brook-commit 2>/dev/null)" != "$JANUS_COMMIT" ]; then
    log "building Janus $JANUS_TAG ($JANUS_COMMIT)"
    build=$(mktemp -d /var/tmp/janus-build.XXXXXX)
    chown janus:janus "$build"
    # Compile unprivileged; only `make install` runs as root.
    as_janus() { setpriv --reuid=janus --regid=janus --init-groups env HOME="$build" "$@"; }
    as_janus git clone -q --depth 1 --branch "$JANUS_TAG" \
        https://github.com/meetecho/janus-gateway.git "$build/src"
    # Pin the commit, not just the tag: a tag can be moved, a commit hash cannot.
    test "$(as_janus git -C "$build/src" rev-parse HEAD)" = "$JANUS_COMMIT"
    (
        cd "$build/src"
        as_janus sh autogen.sh >/dev/null
        # Only what Brook uses: VideoRoom + WebSocket transport (see services/sfu/Dockerfile).
        as_janus ./configure -q --prefix=/opt/janus --disable-docs \
            --disable-all-plugins --enable-plugin-videoroom \
            --disable-all-transports --enable-websockets \
            --disable-all-handlers --disable-all-loggers
        as_janus make -s -j"$(nproc)"
        # `make install` regenerates version.c from git as root, and git refuses a
        # checkout owned by another user ("dubious ownership"), which breaks the
        # link. Trust this one directory for this one command only, via git's env
        # config, so root's global git config is never touched.
        GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=safe.directory GIT_CONFIG_VALUE_0="$build/src" \
            make -s install
    )
    echo "$JANUS_COMMIT" > /opt/janus/.brook-commit
    rm -rf "$build"
fi
install -m 0644 "$SRC/services/sfu/janus.jcfg" "$SRC/services/sfu/janus.plugin.videoroom.jcfg" \
    /opt/janus/etc/janus/
# Native override: binds the Janus API to loopback (see the file's header).
install -m 0644 "$SRC/deploy/native/janus.transport.websockets.jcfg" /opt/janus/etc/janus/
restorecon -R /opt/janus

# ---------------------------------------------------------------- api
log "api"
install -d -m 0755 /opt/brook /opt/brook/api
# Sync in place rather than build-and-swap: a venv's scripts hard-code its own
# path, so a venv built elsewhere and moved here would not run.
rsync -a --delete --exclude .venv --exclude tests --exclude e2e \
    --exclude '.*_cache' --exclude .coverage \
    "$SRC/services/api/" /opt/brook/api/
chown -R root:root /opt/brook/api
# System Python 3.12, never a downloaded one: the interpreter then gets the
# distro's security updates via dnf-automatic like everything else.
UV_PYTHON_DOWNLOADS=never UV_CACHE_DIR=/var/cache/brook-uv \
    uv sync --quiet --locked --no-dev --python /usr/bin/python3.12 --directory /opt/brook/api
restorecon -R /opt/brook

# ---------------------------------------------------------------- PostgreSQL
log "PostgreSQL"
if [ ! -f /var/lib/pgsql/data/PG_VERSION ]; then
    postgresql-setup --initdb
fi
# The api's DB login rests on this line (peer: OS user brook = role brook). Assert
# it rather than trust the packaging default: without it the api never starts.
grep -Eq '^local[[:space:]]+all[[:space:]]+all[[:space:]]+peer' /var/lib/pgsql/data/pg_hba.conf \
    || { echo "pg_hba.conf: expected 'local all all peer'; fix it, then re-run" >&2; exit 1; }
systemctl enable --now postgresql
# The api connects over the local socket with peer auth (OS user brook = role
# brook), so there is no database password to generate, store or leak, and
# Postgres needs no TCP listener at all.
psql_su() { runuser -u postgres -- psql -qAtX "$@"; }
if [ "$(psql_su -c "SHOW listen_addresses")" != "" ]; then
    psql_su -c "ALTER SYSTEM SET listen_addresses = ''"
    systemctl restart postgresql
fi
[ "$(psql_su -c "SELECT 1 FROM pg_roles WHERE rolname='brook'")" = 1 ] || psql_su -c "CREATE ROLE brook LOGIN"
[ "$(psql_su -c "SELECT 1 FROM pg_database WHERE datname='brook'")" = 1 ] || psql_su -c "CREATE DATABASE brook OWNER brook"

# ---------------------------------------------------------------- secrets
log "secrets (/etc/brook, generated once, never leave this host)"
install -d -m 0755 /etc/brook
# Each file is guarded on its own so an interrupted first run heals on re-run.
# The Janus API secret is shared: api.env always takes it from janus.env.
umask 077
if [ ! -f /etc/brook/janus.env ]; then
    cat > /etc/brook/janus.env <<EOF
JANUS_API_SECRET=$(openssl rand -hex 32)
JANUS_PUBLIC_IP=$BROOK_PUBLIC_IP
JANUS_RTP_PORTS=$RTP_PORTS
EOF
fi
if [ ! -f /etc/brook/api.env ]; then
    janus_secret=$(sed -n 's/^JANUS_API_SECRET=//p' /etc/brook/janus.env)
    cat > /etc/brook/api.env <<EOF
BROOK_DATABASE_URL=postgresql+asyncpg://brook@/brook?host=/run/postgresql
BROOK_JWT_SIGNING_KEY=$(openssl rand -hex 32)
BROOK_AUTO_CREATE_SCHEMA=false
BROOK_JANUS_URL=ws://127.0.0.1:8188
BROOK_JANUS_API_SECRET=$janus_secret
EOF
fi
umask 022
chown root:janus /etc/brook/janus.env && chmod 0640 /etc/brook/janus.env
chown root:brook /etc/brook/api.env && chmod 0640 /etc/brook/api.env

# ---------------------------------------------------------------- services
log "systemd units"
install -m 0644 "$SRC/deploy/native/brook-api.service" "$SRC/deploy/native/brook-janus.service" \
    /etc/systemd/system/
systemctl daemon-reload
systemctl enable brook-janus brook-api
# No automatic rollback: /opt/brook/api was just replaced in place and the next
# start migrates. This dump is the way back from a bad migration (README.md
# "Rollback"). Taken before the restart, so it is the pre-migration schema.
install -d -m 0700 -o postgres -g postgres /var/backups/brook
if [ "$(psql_su -d brook -c "SELECT to_regclass('public.alembic_version') IS NOT NULL")" = t ]; then
    runuser -u postgres -- pg_dump -Fc brook > "/var/backups/brook/brook-$(date -u +%Y%m%dT%H%M%SZ).dump"
    # Names are UTC timestamps, so a reverse sort is newest-first; keep the last 5.
    find /var/backups/brook -name 'brook-*.dump' | sort -r | tail -n +6 | xargs -r rm --
fi
systemctl restart brook-janus
systemctl restart brook-api

# Caddyfile is installed on every run; Caddy itself is only reloaded if it is
# already live (fresh installs go live via README.md, after the admin exists).
[ -f /etc/caddy/Caddyfile.dist ] || cp -a /etc/caddy/Caddyfile /etc/caddy/Caddyfile.dist
# This is the one internet-facing config: validate before installing, show any
# change, and reload only when something actually changed.
new_caddyfile=$(mktemp)
sed "s/BROOK_DOMAIN/$BROOK_DOMAIN/" "$SRC/deploy/native/Caddyfile" > "$new_caddyfile"
caddy validate --adapter caddyfile --config "$new_caddyfile" >/dev/null
if ! cmp -s "$new_caddyfile" /etc/caddy/Caddyfile; then
    diff -u /etc/caddy/Caddyfile "$new_caddyfile" || true
    install -m 0644 "$new_caddyfile" /etc/caddy/Caddyfile
    restorecon /etc/caddy/Caddyfile
    if systemctl is-active -q caddy; then systemctl reload caddy; fi
fi
rm -f "$new_caddyfile"

log "health"
for _ in $(seq 20); do
    curl -fsS http://127.0.0.1:8000/health >/dev/null 2>&1 && break
    sleep 1
done
curl -fsS http://127.0.0.1:8000/health && echo
echo "listening sockets (only sshd, caddy, and janus media may be non-loopback):"
ss -Hltunp | awk '{print $1, $5, $7}'
