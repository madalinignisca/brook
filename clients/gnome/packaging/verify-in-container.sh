#!/bin/sh
# Check a release tarball on the distro this runs in (Debian 13, Ubuntu 26.04, Alpine):
# install the runtime packages INSTALL.md lists, run the tarball's own install.sh, then
# make sure that
#   1. every shared library the installed binary needs resolves,
#   2. the GStreamer elements a call needs exist,
#   3. the app starts and stays up under a virtual display.
# Run as root in a throwaway container, from the repo root:
#   docker run --rm -v "$PWD":/w -w /w debian:13 sh clients/gnome/packaging/verify-in-container.sh dist/<tarball>
# POSIX sh: Alpine has no bash.
set -eu
tarball="$1"
. /etc/os-release

# Keep these lists in step with INSTALL.md (plus a virtual display for step 3).
case "$ID" in
  debian|ubuntu)
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      libgtk-4-1 libadwaita-1-0 \
      gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
      gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-nice \
      gstreamer1.0-pipewire gstreamer1.0-tools libssl3t64 liblcms2-2 libseccomp2 \
      libfontconfig1 glycin-loaders bubblewrap \
      adwaita-icon-theme fonts-dejavu-core xvfb xauth dbus >/dev/null
    # Ubuntu 24.04's archive has no GStreamer GTK 4 plugin (measured: "Unable to locate
    # package gstreamer1.0-gtk4"), so calls (the app refuses to start one without it) need it
    # from elsewhere there. Everywhere
    # else it is a package and the element check below requires it.
    if [ "$ID" = ubuntu ] && [ "$VERSION_ID" = 24.04 ]; then
      echo "NOTE     no gstreamer1.0-gtk4 on $PRETTY_NAME: calls need the plugin from elsewhere"
    else
      apt-get install -y -qq --no-install-recommends gstreamer1.0-gtk4 >/dev/null
    fi
    ;;
  alpine)
    apk add -q --no-cache \
      gtk4.0 libadwaita gstreamer gstreamer-tools gst-plugins-base gst-plugins-good \
      gst-plugins-bad gst-plugins-ugly gst-libav libnice-gstreamer gst-plugins-rs-gtk4 \
      gst-plugin-pipewire openssl lcms2 libseccomp fontconfig glycin-loaders-all bubblewrap \
      adwaita-icon-theme font-dejavu xvfb xvfb-run dbus
    ;;
  *) echo "unsupported distro: $ID" >&2; exit 2 ;;
esac
echo "== $PRETTY_NAME"

work="$(mktemp -d)"
export HOME="$work/home" XDG_RUNTIME_DIR="$work/run"
mkdir -p "$HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
tar -C "$work" -xzf "$tarball"

# The documented install path, not a shortcut: INSTALL.md tells users to run ./install.sh,
# and on Alpine (no bash) that was broken once without any check noticing.
echo "== 0. ./install.sh"
(cd "$work"/brook-gnome-*/ && ./install.sh)
bin="$HOME/.local/bin/brook-gnome"
[ -x "$bin" ] || { echo "FAIL: install.sh did not install $bin" >&2; exit 1; }

echo "== 1. shared libraries"
# ldd exits non-zero for a missing or non-ELF file, so check its status as well as its text.
if ! out="$(ldd "$bin" 2>&1)" || echo "$out" | grep -E 'not found|Error'; then
  echo "$out" >&2
  echo "FAIL: the binary can't load its libraries here" >&2; exit 1
fi
echo "ok"

echo "== 2. GStreamer elements a call needs"
missing=0
# Alpine's gst-plugins-bad is built without the webrtc plugin (measured on 3.24: no
# libgstwebrtc.so in any package), so calls can't work there yet. Chat does; say so
# loudly instead of failing the release of a build that is otherwise fine.
soft=""
[ "$ID" = alpine ] && soft="webrtcbin"
[ "$ID" = ubuntu ] && [ "$VERSION_ID" = 24.04 ] && soft="gtk4paintablesink"
for e in webrtcbin nicesrc nicesink dtlssrtpenc dtlssrtpdec srtpenc srtpdec rtpopuspay \
         opusenc opusdec rtph264pay rtph264depay h264parse rtpvp8pay vp8enc vp8dec \
         decodebin videoconvert audioconvert autoaudiosrc autoaudiosink gtk4paintablesink; do
  gst-inspect-1.0 --exists "$e" && continue
  case " $soft " in
    *" $e "*) echo "NOTE     $e is not packaged on $PRETTY_NAME: calls are unavailable" ;;
    *) echo "MISSING  $e"; missing=1 ;;
  esac
done
[ "$missing" = 0 ] && echo "ok"
[ "$missing" = 0 ] || exit 1

echo "== 3. starts and stays up"
# Software rendering: there is no GPU in a container. timeout reports the deadline with 124
# (busybox's, on Alpine, with 143 = 128+SIGTERM). Those codes can also come from an early
# death, so the elapsed time has to show the deadline really passed.
export GSK_RENDERER=cairo
start="$(date +%s)"
set +e
dbus-run-session -- xvfb-run -a timeout 10 "$bin" >"$work/out.log" 2>&1
code=$?
set -e
elapsed=$(( $(date +%s) - start ))
ok=0
[ "$code" -eq 124 ] && ok=1
[ "$ID" = alpine ] && [ "$code" -eq 143 ] && ok=1
if [ "$ok" -ne 1 ] || [ "$elapsed" -lt 9 ]; then
  echo "FAIL: exited with $code after ${elapsed}s, before the 10 s deadline:" >&2
  tail -30 "$work/out.log" >&2
  exit 1
fi
echo "ok (still running after ${elapsed}s)"
# A panic or GTK critical on a worker thread leaves the process alive, so look for them.
if grep -E 'panicked at|-CRITICAL' "$work/out.log"; then
  echo "FAIL: the app logged a panic or critical" >&2; exit 1
fi
# The documented removal must take the app away again (INSTALL.md: ./install.sh --uninstall).
(cd "$work"/brook-gnome-*/ && ./install.sh --uninstall)
[ ! -e "$bin" ] || { echo "FAIL: --uninstall left $bin behind" >&2; exit 1; }
# A launcher left pointing at a deleted binary is the failure users would see.
desktop="${XDG_DATA_HOME:-$HOME/.local/share}/applications/dev.brook.Brook.desktop"
[ ! -e "$desktop" ] || { echo "FAIL: --uninstall left $desktop behind" >&2; exit 1; }
echo "PASS $PRETTY_NAME"
