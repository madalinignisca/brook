#!/usr/bin/env bash
# Install Brook for the current user (no root): the binary to ~/.local/bin and
# the launcher to ~/.local/share/applications, the icon to the hicolor theme.
# Re-run to upgrade; `./install.sh --uninstall` removes all three.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
apps="$data/applications"
icons="$data/icons/hicolor/scalable/apps"

refresh_caches() {
  command -v update-desktop-database >/dev/null && update-desktop-database "$apps" || true
  command -v gtk4-update-icon-cache >/dev/null \
    && gtk4-update-icon-cache -q -t "$data/icons/hicolor" 2>/dev/null || true
}

if [ "${1:-}" = "--uninstall" ]; then
  rm -f "$bin/brook-gnome" "$apps/dev.brook.Brook.desktop" "$icons/dev.brook.Brook.svg"
  refresh_caches
  echo "Brook removed."
  exit 0
fi

mkdir -p "$bin" "$apps" "$icons"
install -m 0755 "$here/brook-gnome" "$bin/brook-gnome"
install -m 0644 "$here/dev.brook.Brook.svg" "$icons/dev.brook.Brook.svg"
sed "s|^Exec=brook-gnome|Exec=$bin/brook-gnome|" "$here/dev.brook.Brook.desktop" \
  > "$apps/dev.brook.Brook.desktop"
refresh_caches
echo "Installed Brook $(cat "$here/VERSION") to $bin/brook-gnome."
echo "Start it from your app launcher, or run: brook-gnome"
