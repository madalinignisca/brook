#!/bin/sh
# Build the musl (Alpine) tarball. Runs as root INSIDE an alpine container, from the repo root:
#   docker run --rm -v "$PWD":/w -w /w alpine:3.24 sh clients/gnome/packaging/build-alpine.sh
# then, on the host (make-tarball.sh needs bash, which Alpine lacks):
#   clients/gnome/packaging/make-tarball.sh <version> <arch> dist musl
# POSIX sh: Alpine has no bash. The toolchain is Alpine's own (rust/cargo from apk), so the
# binary links against the same musl, GTK and GStreamer it will run against.
set -eu
apk add -q --no-cache build-base pkgconf rust cargo \
  gtk4.0-dev libadwaita-dev gstreamer-dev gst-plugins-base-dev gst-plugins-bad-dev \
  openssl-dev lcms2-dev libseccomp-dev fontconfig-dev

# musl defaults to a fully static C runtime, which cannot dlopen or link against the
# shared glib/GTK stack. Keep it apart from a glibc build's target/release.
export RUSTFLAGS="-C target-feature=-crt-static"
export CARGO_TARGET_DIR="$PWD/target/musl"
cargo build --release --locked -p brook-gnome

# Where make-tarball.sh looks for the binary.
mkdir -p target/release
cp "$CARGO_TARGET_DIR/release/brook-gnome" target/release/brook-gnome
