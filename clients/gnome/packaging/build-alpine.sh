#!/bin/sh
# Build the musl (Alpine) binary. Runs as root INSIDE an alpine container, from the repo root:
#   docker run --rm -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" -v "$PWD":/w -w /w \
#     alpine:3.24 sh clients/gnome/packaging/build-alpine.sh
# then, on the host (make-tarball.sh needs bash, which Alpine lacks):
#   BROOK_BINARY=target/musl/release/brook-gnome clients/gnome/packaging/make-tarball.sh <version> <arch> dist musl
# POSIX sh: Alpine has no bash. The toolchain is Alpine's own (rust/cargo from apk), so the
# binary links against the same musl, GTK and GStreamer it will run against.
set -eu

# The container is root: hand the build output back to the caller, or the next host
# `cargo build` or cleanup hits permission errors. Runs on failure too.
trap 'chown -R "${HOST_UID:-0}:${HOST_GID:-0}" target 2>/dev/null || true' EXIT

apk add -q --no-cache build-base pkgconf rust cargo \
  gtk4.0-dev libadwaita-dev gstreamer-dev gst-plugins-base-dev gst-plugins-bad-dev \
  openssl-dev lcms2-dev libseccomp-dev fontconfig-dev

# musl defaults to a fully static C runtime, which cannot dlopen or link against the
# shared glib/GTK stack. target/musl keeps it apart from a glibc build's target/release.
export RUSTFLAGS="-C target-feature=-crt-static"
export CARGO_TARGET_DIR="$PWD/target/musl"
cargo build --release --locked -p brook-gnome
