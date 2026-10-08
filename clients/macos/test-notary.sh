#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
# SPDX-License-Identifier: AGPL-3.0-or-later

# Tests for the notarization scripts (#68), run by hand: no Apple, no build.
#   clients/macos/test-notary.sh
# notary-lib.sh against a fake `xcrun`, and check-notarizable.sh on small synthetic apps. Each
# case states what it would catch; a failure prints which and exits 1.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/notary-lib.sh"
pass=0; fail=0
ok()   { pass=$((pass + 1)); }
bad()  { fail=$((fail + 1)); echo "FAIL: $1" >&2; }
check() { # name, expected exit, actual exit
  if [[ "$2" == "$3" ]]; then ok; else bad "$1 (expected exit $2, got $3)"; fi
}
has() { # name, haystack, needle
  if [[ "$2" == *"$3"* ]]; then ok; else bad "$1 (no '$3' in: ${2:0:200})"; fi
}
lacks() {
  if [[ "$2" != *"$3"* ]]; then ok; else bad "$1 (unexpected '$3')"; fi
}
T="$(mktemp -d)"; trap 'chmod -R u+rwx "$T" 2>/dev/null; rm -rf "$T"' EXIT

# ---- the profile name -------------------------------------------------------
for case in \
  'BROOK_NOTARY_PROFILE = my-profile|my-profile' \
  'BROOK_NOTARY_PROFILE = my-profile   // the notary one|my-profile' \
  $'BROOK_NOTARY_PROFILE = my-profile  \r|my-profile' \
  'BROOK_NOTARY_PROFILE =   |' \
  'OTHER = x|'; do
  printf '%s\n' "${case%|*}" > "$T/x.xcconfig"
  got="$(read_notary_profile "$T/x.xcconfig")"
  [[ "$got" == "${case##*|}" ]] && ok || bad "profile name from '${case%|*}': got [$got]"
done
[[ -z "$(read_notary_profile "$T/none.xcconfig")" ]] && ok || bad "a missing file"

# ---- submit, then staple only when Accepted ---------------------------------
export CALLS="$T/calls"
# A fake `xcrun` reading these: what `submit` prints and returns, and what `log` prints.
xcrun() {
  echo "$*" >> "$CALLS"
  case "$2" in
    submit) echo "$SUBMIT_JSON"; return "$SUBMIT_RC" ;;
    log) echo "$LOG_TEXT" ;;
    *) return 0 ;;
  esac
}
fake_xcrun() { SUBMIT_JSON="$1"; SUBMIT_RC="$2"; LOG_TEXT="$3"; : > "$CALLS"; }

fake_xcrun '{"id":"abc","status":"Accepted"}' 0 ''; out="$(notarize_app app.app app.zip p 2>&1)"; rc=$?
check "accepted succeeds" 0 "$rc"; has "accepted staples" "$(cat "$CALLS")" "stapler staple"; has "accepted validates" "$(cat "$CALLS")" "stapler validate"

fake_xcrun '{"id":"def","status":"Invalid"}' 0 'LOG-FOR-DEF'; out="$(notarize_app app.app app.zip p 2>&1)"; rc=$?
check "a rejection fails" 1 "$rc"; has "a rejection names its id" "$out" "submission def"; has "a rejection shows the log" "$out" "LOG-FOR-DEF"
lacks "a rejection never staples" "$(cat "$CALLS")" "stapler"

fake_xcrun 'Error: network down' 1 ''; out="$(notarize_app app.app app.zip p 2>&1)"; rc=$?
check "output that isn't JSON fails" 1 "$rc"; has "it shows what notarytool said" "$out" "network down"
lacks "it never staples" "$(cat "$CALLS")" "stapler"

fake_xcrun '{"id":"ghi","status":"Invalid"}' 69 'LOG-FOR-GHI'; out="$(notarize_app app.app app.zip p 2>&1)"; rc=$?
check "a nonzero exit with JSON fails" 1 "$rc"; has "it keeps the diagnosis" "$out" "LOG-FOR-GHI"

# A nonzero exit with an Accepted body must still not staple (the call itself failed).
fake_xcrun '{"id":"jkl","status":"Accepted"}' 3 ''; out="$(notarize_app app.app app.zip p 2>&1)"; rc=$?
check "a failed call is never a success, whatever it printed" 1 "$rc"; lacks "and never staples" "$(cat "$CALLS")" "stapler"

# ---- check-notarizable.sh on synthetic apps ----------------------------------
CN="$HERE/check-notarizable.sh"
mkdir -p "$T/a/Brook.app/Contents/MacOS"; cp /bin/ls "$T/a/Brook.app/Contents/MacOS/Brook"
out="$("$CN" "$T/a/Brook.app" 2>&1)"; rc=$?
check "a binary without a Developer ID signature is refused" 1 "$rc"; has "it says why" "$out" "not signed by a Developer ID"

mkdir -p "$T/b/Brook.app/Contents"; cp /bin/ls "$T/b/Brook.app/Contents/hidden.dat"; chmod 644 "$T/b/Brook.app/Contents/hidden.dat"
out="$("$CN" "$T/b/Brook.app" 2>&1)"; rc=$?
check "a non-executable Mach-O under another name is still found" 1 "$rc"; has "it names the file" "$out" "hidden.dat"

mkdir -p "$T/c/Brook.app/Contents/Resources"; echo text > "$T/c/Brook.app/Contents/Resources/a.txt"
out="$("$CN" "$T/c/Brook.app" 2>&1)"; rc=$?
check "an app with no binaries is an error, not a pass" 1 "$rc"; has "it says so" "$out" "no binaries"

mkdir -p "$T/d/Brook.app/Contents/MacOS" "$T/d/Brook.app/Contents/locked"; cp /bin/ls "$T/d/Brook.app/Contents/MacOS/Brook"; chmod 000 "$T/d/Brook.app/Contents/locked"
out="$("$CN" "$T/d/Brook.app" 2>&1)"; rc=$?
chmod 755 "$T/d/Brook.app/Contents/locked"
check "a walk that fails is an error, not a partial pass" 1 "$rc"; has "it says it couldn't walk" "$out" "couldn't walk"

out="$("$CN" "$T/nope/Brook.app" 2>&1)"; rc=$?
check "a missing app is an error" 1 "$rc"; has "and says it doesn't exist" "$out" "no such app"

echo "test-notary: $pass passed, $fail failed"
[[ "$fail" -eq 0 ]]
