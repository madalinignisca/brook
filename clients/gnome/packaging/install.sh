#!/usr/bin/env bash
# Install Brook for the current user (no root): the binary to ~/.local/bin and
# the launcher to ~/.local/share/applications. Re-run to upgrade;
# `./install.sh --uninstall` removes both.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"

if [ "${1:-}" = "--uninstall" ]; then
  rm -f "$bin/brook-gnome" "$apps/dev.brook.Brook.desktop"
  echo "Brook removed."
  exit 0
fi

mkdir -p "$bin" "$apps"
install -m 0755 "$here/brook-gnome" "$bin/brook-gnome"
sed "s|^Exec=brook-gnome|Exec=$bin/brook-gnome|" "$here/dev.brook.Brook.desktop" \
  > "$apps/dev.brook.Brook.desktop"
command -v update-desktop-database >/dev/null && update-desktop-database "$apps" || true
echo "Installed Brook $(cat "$here/VERSION") to $bin/brook-gnome."
echo "Start it from your app launcher, or run: brook-gnome"
