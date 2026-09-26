#!/usr/bin/env bash
# Check a built Brook.app's image decoder (previews spec §5): the broker and the worker carry
# exactly their entitlements, the Debug test kinds are compiled out of Release, and the whole
# bundle's signature verifies. `release` also requires the hardened runtime and a team.
#   clients/macos/check-decoder.sh <path/to/Brook.app> [release]
set -euo pipefail

app="$1"
xpc="$app/Contents/XPCServices/BrookImageDecoder.xpc"
worker="$xpc/Contents/MacOS/BrookImageWorker"
broker="$xpc/Contents/MacOS/BrookImageDecoder"
fail() { echo "check-decoder: $*" >&2; exit 1; }

keys() { codesign -d --entitlements - --xml "$1" 2>/dev/null | plutil -convert json -o - - | /usr/bin/python3 -c 'import json,sys; print(" ".join(sorted(json.load(sys.stdin))))'; }
[ "$(keys "$xpc")" = "com.apple.security.app-sandbox" ] || fail "broker entitlements: $(keys "$xpc")"
[ "$(keys "$worker")" = "com.apple.security.app-sandbox com.apple.security.inherit" ] \
  || fail "worker entitlements: $(keys "$worker")"
codesign --verify --deep --strict "$app" || fail "the bundle's signature doesn't verify"

if [[ "${2:-}" == "release" ]]; then
  for bin in "$broker" "$worker"; do
    # The test kinds' names and the probe's report keys must not be in a Release binary.
    if strings -a "$bin" | grep -Ec 'probeReport|brokerClient|writeHome|reaped=|group=' >/dev/null; then
      fail "a Debug test hook is in $(basename "$bin")"
    fi
  done
  for code in "$xpc" "$worker"; do
    # Captured first: under pipefail, `grep -q` quitting early would fail codesign by SIGPIPE.
    info="$(codesign -dv "$code" 2>&1)"
    grep -q 'flags=.*runtime' <<<"$info" || fail "no hardened runtime: $code"
    grep -Eq '^TeamIdentifier=[A-Z0-9]{10}$' <<<"$info" || fail "no team: $code"
  done
fi
echo "check-decoder: ok${2:+ ($2)}"
