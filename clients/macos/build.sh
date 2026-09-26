#!/usr/bin/env bash
# Build the macOS client: fresh BrookCore xcframework (so the app never links a stale core),
# regenerate the Xcode project, build. Pass `test` to also run the unit tests.
#   clients/macos/build.sh          → clients/macos/build/Build/Products/Debug/Brook.app
#   clients/macos/build.sh test
#   clients/macos/build.sh release → …/Release/Brook.app (hardened runtime, shipped entitlements only)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
command -v xcodegen >/dev/null || { echo "xcodegen not found (brew install xcodegen)" >&2; exit 1; }

"$ROOT/bindings/apple/build-xcframework.sh"
(cd "$HERE" && xcodegen --quiet)

args=(build)
config=Debug
if [[ "${1:-}" == "release" ]]; then
  config=Release
  # Ad-hoc Release cannot launch (library validation vs. the embedded WebRTC.framework).
  grep -qs "^DEVELOPMENT_TEAM *= *[A-Z0-9]" "$HERE/Local.xcconfig" || {
    echo "Release needs a signing identity: see clients/macos/Signing.xcconfig" >&2; exit 1; }
fi
# A test that deadlocks must fail, not hang the run: cap each test at 60 s.
[[ "${1:-}" == "test" ]] && args=(test -test-timeouts-enabled YES -maximum-test-execution-time-allowance 60)
xcodebuild -project "$HERE/Brook.xcodeproj" -scheme Brook -configuration "$config" \
  -derivedDataPath "$HERE/build" "${args[@]}"

# The image decoder's sandbox and signing (previews spec §5), on every plain build.
if [[ "${1:-}" != "test" ]]; then
  "$HERE/check-decoder.sh" "$HERE/build/Build/Products/$config/Brook.app" \
    $([[ "$config" == Release ]] && echo release)
fi

echo "app: $HERE/build/Build/Products/$config/Brook.app"
