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

args=(build)
# A test that deadlocks must fail, not hang the run: cap each test at 60 s.
[[ "${1:-}" == "test" ]] && args=(test -test-timeouts-enabled YES -maximum-test-execution-time-allowance 60)
xcodebuild -project "$HERE/Brook.xcodeproj" -scheme Brook -configuration Debug \
  -derivedDataPath "$HERE/build" "${args[@]}"

echo "app: $HERE/build/Build/Products/Debug/Brook.app"
