#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
# SPDX-License-Identifier: AGPL-3.0-or-later

# Build the macOS client: fresh BrookCore xcframework (so the app never links a stale core),
# regenerate the Xcode project, build. Pass `test` to also run the unit tests.
#   clients/macos/build.sh          → clients/macos/build.noindex/Build/Products/Debug/Brook.app
#   clients/macos/build.sh test
#   clients/macos/build.sh release → …/Release/Brook.app (hardened runtime, shipped entitlements only)
#   clients/macos/build.sh install  → release, then copies it to ${BROOK_INSTALL_DIR:-/Applications}/Brook.app
#     (refuses while Brook runs). The build output is build.noindex so Spotlight (for builds made
#     with this script) finds only that app.
#   clients/macos/build.sh notarize → release, then Apple's notary, stapling and a zip to ship (#68)
#     Needs, in the gitignored Local.xcconfig, the name of a `notarytool store-credentials` profile:
#       BROOK_NOTARY_PROFILE = <profile name>
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
# clients/ios/build.sh shares this script's output (the xcframework and generated Swift), so
# both take one lock for their whole run; the second waits. The kernel holds it, so a killed
# run releases it.
[[ -n "${BROOK_APPLE_LOCKED:-}" ]] || BROOK_APPLE_LOCKED=1 exec /usr/bin/lockf -k "$ROOT/bindings/apple/.build.lock" "$HERE/$(basename "$0")" "$@"
command -v xcodegen >/dev/null || { echo "xcodegen not found (brew install xcodegen)" >&2; exit 1; }
# The install path is destructive: its tests (fast, no build) run before anything is built.
[[ "${1:-}" == "test" ]] && { "$HERE/test-install.sh" || exit 1; }
# Every person is shown through PersonName (Show usernames, #238): cheap, so before the build too.
[[ "${1:-}" == "test" ]] && { "$HERE/check-person-names.sh" || exit 1; }

args=(build)
config=Debug
source "$HERE/notary-lib.sh"
source "$HERE/install-lib.sh"
migrate_build_dir "$HERE"
mode="${1:-}"
notary_profile=""
if [[ "$mode" == "release" || "$mode" == "notarize" || "$mode" == "install" ]]; then
  config=Release
  # Ad-hoc Release cannot launch (library validation vs. the embedded WebRTC.framework).
  grep -qs "^DEVELOPMENT_TEAM *= *[A-Z0-9]" "$HERE/Local.xcconfig" || {
    echo "Release needs a signing identity: see clients/macos/Signing.xcconfig" >&2; exit 1; }
fi
if [[ "$mode" == "notarize" ]]; then
  # Before the long build: the profile's name (the credentials themselves stay in the keychain).
  notary_profile="$(read_notary_profile "$HERE/Local.xcconfig")"
  [[ -n "$notary_profile" ]] || {
    echo "notarize needs BROOK_NOTARY_PROFILE in Local.xcconfig (see the header of build.sh)" >&2; exit 1; }
fi
if [[ "$mode" == "install" ]]; then
  # Before the long builds too; install_app checks again, right before the swap.
  refuse_if_brook_running || exit 1
fi
# After the checks above, so a missing setting fails at once, not after the slow builds.
"$ROOT/bindings/apple/build-xcframework.sh"
(cd "$HERE" && xcodegen --quiet)

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
  -derivedDataPath "$HERE/build.noindex" "${args[@]}"

# The image decoder's sandbox and signing (previews spec §5), on every plain build.
if [[ "${1:-}" != "test" ]]; then
  "$HERE/check-decoder.sh" "$HERE/build.noindex/Build/Products/$config/Brook.app" \
    $([[ "$config" == Release ]] && echo release)
fi

app="$HERE/build.noindex/Build/Products/$config/Brook.app"

# Release: every binary must be Developer ID signed with the hardened runtime and a secure
# timestamp, or Apple's notary rejects it (#68). Caught here, offline, in seconds.
[[ "$config" == Release ]] && "$HERE/check-notarizable.sh" "$app"

if [[ "$mode" == "notarize" ]]; then
  zip="$HERE/build.noindex/Brook-notarize.zip"
  ditto -c -k --keepParent "$app" "$zip"
  notarize_app "$app" "$zip" "$notary_profile" || exit 1
  spctl -a -vvv "$app"
  rm -f "$zip"
  # What to ship: the stapled app, zipped (the stapled ticket lets it open offline).
  ditto -c -k --keepParent "$app" "$HERE/build.noindex/Brook.zip"
  echo "notarized: $HERE/build.noindex/Brook.zip"
fi

if [[ "$mode" == "install" ]]; then
  install_app "$app" || exit 1
fi

echo "app: $app"
