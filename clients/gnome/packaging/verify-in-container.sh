#!/bin/sh
# Check a release tarball on the distro this runs in (Debian 13, Ubuntu 26.04, Alpine):
# install the runtime packages INSTALL.md lists, then make sure that
#   1. every shared library the binary needs resolves,
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
      gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-nice gstreamer1.0-gtk4 \
      gstreamer1.0-pipewire gstreamer1.0-tools libssl3t64 liblcms2-2 libseccomp2 \
      libfontconfig1 glycin-loaders bubblewrap \
      adwaita-icon-theme fonts-dejavu-core xvfb xauth dbus >/dev/null
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
tar -C "$work" -xzf "$tarball"
bin="$(echo "$work"/brook-gnome-*/brook-gnome)"

echo "== 1. shared libraries"
if ldd "$bin" 2>&1 | grep -E 'not found|version .*not found|Error'; then
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
for e in webrtcbin nicesrc nicesink dtlssrtpenc dtlssrtpdec srtpenc srtpdec rtpopuspay \
         opusenc opusdec rtph264pay rtph264depay h264parse rtpvp8pay vp8enc vp8dec \
         decodebin videoconvert audioconvert autoaudiosrc autoaudiosink; do
  gst-inspect-1.0 --exists "$e" && continue
  case " $soft " in
    *" $e "*) echo "NOTE     $e is not packaged on $PRETTY_NAME: calls are unavailable" ;;
    *) echo "MISSING  $e"; missing=1 ;;
  esac
done
[ "$missing" = 0 ] && echo "ok"
# Informational: the video sink is a separate package on some distros.
gst-inspect-1.0 --exists gtk4paintablesink && echo "gtk4paintablesink: present" \
  || echo "note: gtk4paintablesink not installed (call video needs it)"
[ "$missing" = 0 ] || exit 1

echo "== 3. starts and stays up"
# Software rendering: there is no GPU in a container. timeout exits 124 when the app was
# still running at the deadline (busybox's, on Alpine, reports 143 = 128+SIGTERM instead);
# that is what we want, anything else is a crash.
export GSK_RENDERER=cairo HOME="$work/home" XDG_RUNTIME_DIR="$work/run"
mkdir -p "$HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
set +e
dbus-run-session -- xvfb-run -a timeout 10 "$bin" >"$work/out.log" 2>&1
code=$?
set -e
if [ "$code" -ne 124 ] && [ "$code" -ne 143 ]; then
  echo "FAIL: exited with $code before the 10 s deadline:" >&2
  tail -30 "$work/out.log" >&2
  exit 1
fi
echo "ok (still running after 10 s)"
grep -i -E 'critical|panic' "$work/out.log" | head -5 || true
echo "PASS $PRETTY_NAME"
