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

dnf -y -q install firewalld

zone=public
firewall-offline-cmd --set-default-zone="$zone" >/dev/null
firewall-offline-cmd --zone="$zone" --add-service=ssh >/dev/null        # admin + git (TCP 22)
firewall-offline-cmd --zone="$zone" --add-service=http >/dev/null       # ACME HTTP-01 + redirect
firewall-offline-cmd --zone="$zone" --add-service=https >/dev/null      # TCP 443
firewall-offline-cmd --zone="$zone" --add-port=443/udp >/dev/null       # HTTP/3
firewall-offline-cmd --zone="$zone" --add-port=20000-20099/udp >/dev/null  # Janus media
firewall-offline-cmd --zone="$zone" --remove-service=cockpit >/dev/null 2>&1 || true

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
