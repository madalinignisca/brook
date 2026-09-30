#!/usr/bin/env bash
# Build the macOS client: fresh BrookCore xcframework (so the app never links a stale core),
# regenerate the Xcode project, build. Pass `test` to also run the unit tests.
#   clients/macos/build.sh          → clients/macos/build/Build/Products/Debug/Brook.app
#   clients/macos/build.sh test
#   clients/macos/build.sh release → …/Release/Brook.app (hardened runtime, shipped entitlements only)
#   clients/macos/build.sh notarize → release, then Apple's notary, stapling and a zip to ship (#68)
#     Needs, in the gitignored Local.xcconfig, the name of a `notarytool store-credentials` profile:
#       BROOK_NOTARY_PROFILE = <profile name>
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
command -v xcodegen >/dev/null || { echo "xcodegen not found (brew install xcodegen)" >&2; exit 1; }

args=(build)
config=Debug
mode="${1:-}"
notary_profile=""
if [[ "$mode" == "release" || "$mode" == "notarize" ]]; then
  config=Release
  # Ad-hoc Release cannot launch (library validation vs. the embedded WebRTC.framework).
  grep -qs "^DEVELOPMENT_TEAM *= *[A-Z0-9]" "$HERE/Local.xcconfig" || {
    echo "Release needs a signing identity: see clients/macos/Signing.xcconfig" >&2; exit 1; }
fi
if [[ "$mode" == "notarize" ]]; then
  # Before the long build: the profile's name (the credentials themselves stay in the keychain).
  # The name only: a trailing `// comment`, spaces or a CRLF would otherwise become part of it and
  # fail after the whole build.
  notary_profile="$(sed -n 's/^BROOK_NOTARY_PROFILE *= *//p' "$HERE/Local.xcconfig" 2>/dev/null | head -1 \
    | sed -e 's://.*$::' -e 's/[[:space:]]*$//' | tr -d '\r')"
  [[ -n "$notary_profile" ]] || {
    echo "notarize needs BROOK_NOTARY_PROFILE in Local.xcconfig (see the header of build.sh)" >&2; exit 1; }
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
  -derivedDataPath "$HERE/build" "${args[@]}"

# The image decoder's sandbox and signing (previews spec §5), on every plain build.
if [[ "${1:-}" != "test" ]]; then
  "$HERE/check-decoder.sh" "$HERE/build/Build/Products/$config/Brook.app" \
    $([[ "$config" == Release ]] && echo release)
fi

app="$HERE/build/Build/Products/$config/Brook.app"

# Release: every binary must be Developer ID signed with the hardened runtime and a secure
# timestamp, or Apple's notary rejects it (#68). Caught here, offline, in seconds.
[[ "$config" == Release ]] && "$HERE/check-notarizable.sh" "$app"

if [[ "$mode" == "notarize" ]]; then
  zip="$HERE/build/Brook-notarize.zip"
  ditto -c -k --keepParent "$app" "$zip"
  # JSON, so a rejection is caught here with its log, not as a confusing stapler failure after it.
  # The exit status is kept, not left to `set -e`: a failed call's output is the diagnosis.
  notary_rc=0
  result="$(xcrun notarytool submit "$zip" --keychain-profile "$notary_profile" --wait --output-format json)" || notary_rc=$?
  # "id|status" (a separator that isn't whitespace, so an empty id can't shift the status).
  parsed="$(/usr/bin/python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("id","") + "|" + d.get("status",""))' <<<"$result" 2>/dev/null)" || parsed="|"
  IFS='|' read -r sub_id sub_status <<<"$parsed" || true
  if [[ "$notary_rc" -ne 0 || "$sub_status" != "Accepted" ]]; then
    echo "notarization failed (status: ${sub_status:-unknown}, notarytool exit $notary_rc), submission ${sub_id:-?}" >&2
    echo "$result" >&2
    [[ -n "$sub_id" ]] && xcrun notarytool log "$sub_id" --keychain-profile "$notary_profile" >&2 || true
    exit 1
  fi
  xcrun stapler staple "$app"
  xcrun stapler validate "$app"
  spctl -a -vvv "$app"
  rm -f "$zip"
  # What to ship: the stapled app, zipped (the stapled ticket lets it open offline).
  ditto -c -k --keepParent "$app" "$HERE/build/Brook.zip"
  echo "notarized: $HERE/build/Brook.zip"
fi

echo "app: $app"
