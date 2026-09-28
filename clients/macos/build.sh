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
# A Developer ID profile (#79): the keychain entitlements join the generated (gitignored)
# Brook.entitlements for this build only, so they can't drift from the rest. XcodeGen writes
# that file's path into every configuration, so the file itself is what changes. Release
# only, and regenerated right after: a Debug or test build, even one started from Xcode, never
# carries them (an ad-hoc app with a keychain entitlement is killed at launch).
if [[ "$config" == Release ]] && grep -qs "^BROOK_APP_PROFILE *= *[^ ]" "$HERE/Local.xcconfig"; then
  # Restored however this script ends: a failure, Ctrl-C (the INT trap makes bash exit, so the
  # EXIT one runs) or success. Set before the first edit.
  trap '(cd "$HERE" && xcodegen --quiet)' EXIT
  trap 'exit 130' INT TERM
  ents="$HERE/Brook/Brook.entitlements"
  pb() { /usr/libexec/PlistBuddy -c "$1" "$ents" >/dev/null; }
  pb 'Add :com.apple.application-identifier string $(DEVELOPMENT_TEAM).$(PRODUCT_BUNDLE_IDENTIFIER)'
  pb 'Add :com.apple.developer.team-identifier string $(DEVELOPMENT_TEAM)'
  pb 'Add :keychain-access-groups array'
  pb 'Add :keychain-access-groups: string $(DEVELOPMENT_TEAM).dev.brook.shared'
  # Never a profile build without the group: it would ship signed in by hand each launch.
  /usr/libexec/PlistBuddy -c 'Print :keychain-access-groups:0' "$ents" | grep -q '\.dev\.brook\.shared$' \
    || { echo "the keychain group didn't reach $ents" >&2; exit 1; }
fi
xcodebuild -project "$HERE/Brook.xcodeproj" -scheme Brook -configuration "$config" \
  -derivedDataPath "$HERE/build" "${args[@]}"

# The image decoder's sandbox and signing (previews spec §5), on every plain build.
if [[ "${1:-}" != "test" ]]; then
  "$HERE/check-decoder.sh" "$HERE/build/Build/Products/$config/Brook.app" \
    $([[ "$config" == Release ]] && echo release)
fi

echo "app: $HERE/build/Build/Products/$config/Brook.app"
