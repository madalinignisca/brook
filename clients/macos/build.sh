#!/usr/bin/env bash
# Build the macOS client: fresh BrookCore xcframework (so the app never links a stale core),
# regenerate the Xcode project, build. Pass `test` to also run the unit tests.
#   clients/macos/build.sh          → clients/macos/build/Build/Products/Debug/Brook.app
#   clients/macos/build.sh test
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
command -v xcodegen >/dev/null || { echo "xcodegen not found (brew install xcodegen)" >&2; exit 1; }

"$ROOT/bindings/apple/build-xcframework.sh"
(cd "$HERE" && xcodegen --quiet)

action=build
[[ "${1:-}" == "test" ]] && action=test
xcodebuild -project "$HERE/Brook.xcodeproj" -scheme Brook -configuration Debug \
  -derivedDataPath "$HERE/build" "$action"

echo "app: $HERE/build/Build/Products/Debug/Brook.app"
