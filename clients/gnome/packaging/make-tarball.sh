#!/usr/bin/env bash
# Package a release build of the GNOME client as a tarball.
# Usage: make-tarball.sh <version> <arch> <outdir>   (run from the repo root,
# after `cargo build --release --locked -p brook-gnome`).
set -euo pipefail
version="$1" arch="$2" out="$3"
name="brook-gnome-${version}-linux-${arch}"
stage="$(mktemp -d)/${name}"
mkdir -p "$stage" "$out"

install -m 0755 target/release/brook-gnome "$stage/brook-gnome"
install -m 0644 clients/gnome/data/dev.brook.Brook.desktop "$stage/"
install -m 0755 clients/gnome/packaging/install.sh "$stage/install.sh"
install -m 0644 clients/gnome/packaging/INSTALL.md "$stage/INSTALL.md"
install -m 0644 LICENSE "$stage/LICENSE"
printf '%s\n' "$version" > "$stage/VERSION"

tar -C "$(dirname "$stage")" -czf "$out/${name}.tar.gz" "$name"
(cd "$out" && sha256sum "${name}.tar.gz" > "${name}.tar.gz.sha256")
echo "$out/${name}.tar.gz"
