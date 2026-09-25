#!/bin/bash
# Enable firewalld on the Brook host with only the ports Brook and SSH need.
#
# firewalld is OFF on the AlmaLinux 10 OCI images (the OCI security list is the
# only filter). This turns it on as a second layer. Because this host also serves
# git over SSH, a mistake here would lock out both the admin and git, so:
#   1. the zone is configured OFFLINE, before firewalld ever starts, with ssh in it;
#   2. a dead-man timer stops firewalld after 5 minutes unless cancelled;
#   3. firewalld is only STARTED, not enabled: a reboot also undoes it;
#   4. the operator verifies a NEW ssh session (and git) from outside, then keeps
#      it with `systemctl stop brook-fw-deadman.timer && systemctl enable firewalld`.
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }

# firewall-offline-cmd refuses to run against a live daemon. Once firewalld is
# up, change rules with firewall-cmd --permanent + --reload instead of re-running.
if systemctl is-active -q firewalld; then
    echo "firewalld is already running; use firewall-cmd, not this script" >&2
    exit 1
fi
dnf -y -q install firewalld
# The package's systemd preset ENABLES firewalld on install. Undo that at once:
# otherwise the next reboot starts it with the stock zone (ssh only), silently
# cutting off the site and calls, and it bypasses the dead-man switch below.
systemctl disable -q firewalld

zone=public
# firewall-offline-cmd exits non-zero on no-op changes (ZONE_ALREADY_SET, ...),
# which aborts under `set -e`, so query first and change only what differs.
[ "$(firewall-offline-cmd --get-default-zone)" = "$zone" ] \
    || firewall-offline-cmd --set-default-zone="$zone" >/dev/null
allow_service() {
    firewall-offline-cmd --zone="$zone" --query-service="$1" >/dev/null \
        || firewall-offline-cmd --zone="$zone" --add-service="$1" >/dev/null
}
allow_port() {
    firewall-offline-cmd --zone="$zone" --query-port="$1" >/dev/null \
        || firewall-offline-cmd --zone="$zone" --add-port="$1" >/dev/null
}
allow_service ssh             # admin + git (TCP 22)
allow_service http            # ACME HTTP-01 + redirect
allow_service https           # TCP 443
allow_port 443/udp            # HTTP/3
allow_port 20000-20099/udp    # Janus media
if firewall-offline-cmd --zone="$zone" --query-service=cockpit >/dev/null; then
    firewall-offline-cmd --zone="$zone" --remove-service=cockpit >/dev/null
fi

echo "zone $zone will allow:"
firewall-offline-cmd --zone="$zone" --list-all

# Dead man's switch: if we lose access, firewalld goes away by itself.
systemd-run --unit=brook-fw-deadman --on-active=5min /usr/bin/systemctl stop firewalld
systemctl start firewalld

cat <<'EOF'

firewalld is ON. Within 5 minutes, from skynet:
  ssh busuioc true
  git ls-remote ssh://a1git/srv/git/stilbag-magento.git   # expect HEAD cd75c54
Then keep the firewall (and make it survive reboots):
  ssh busuioc 'sudo systemctl stop brook-fw-deadman.timer && sudo systemctl enable firewalld'
If you do nothing, firewalld stops itself after 5 minutes and is not enabled.
EOF
