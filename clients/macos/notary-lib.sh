#!/usr/bin/env bash
# The notarization steps of build.sh, as functions so test-notary.sh can run them against a fake
# `xcrun` (#68). Sourced, never run.

# The name of the `notarytool store-credentials` profile in a Local.xcconfig: only the name (a
# trailing `// comment`, spaces or a CR would otherwise become part of it and fail after the whole
# build). Empty when there's none.
read_notary_profile() {
  sed -n 's/^BROOK_NOTARY_PROFILE *= *//p' "$1" 2>/dev/null | head -1 \
    | sed -e 's://.*$::' -e 's/[[:space:]]*$//' | tr -d '\r'
}

# Submit $2 (a zip of $1) with profile $3, and on Accepted staple and validate $1. Anything else
# (a rejection, a failed call, output that isn't JSON) prints what notarytool said and its log,
# and returns 1 without stapling.
notarize_app() {
  local app="$1" zip="$2" profile="$3" rc=0 result parsed sub_id="" sub_status=""
  # The exit status is kept, not left to `set -e`: a failed call's output is the diagnosis.
  result="$(xcrun notarytool submit "$zip" --keychain-profile "$profile" --wait --output-format json)" || rc=$?
  # "id|status" (a separator that isn't whitespace, so an empty id can't shift the status).
  parsed="$(/usr/bin/python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("id","") + "|" + d.get("status",""))' <<<"$result" 2>/dev/null)" || parsed="|"
  IFS='|' read -r sub_id sub_status <<<"$parsed" || true
  if [[ "$rc" -ne 0 || "$sub_status" != "Accepted" ]]; then
    echo "notarization failed (status: ${sub_status:-unknown}, notarytool exit $rc), submission ${sub_id:-?}" >&2
    echo "$result" >&2
    [[ -n "$sub_id" ]] && xcrun notarytool log "$sub_id" --keychain-profile "$profile" >&2 || true
    return 1
  fi
  xcrun stapler staple "$app" || return 1
  xcrun stapler validate "$app" || return 1
}
