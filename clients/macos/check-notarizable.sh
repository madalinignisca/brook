#!/usr/bin/env bash
# Whether a built Brook.app would pass Apple's notary checks that can be told locally (#68): every
# Mach-O binary in the bundle is signed by a Developer ID with the hardened runtime and a secure
# timestamp, and none carries get-task-allow. It can't say the notary will accept it (that's
# `build.sh notarize`), but it catches the usual rejections in seconds and offline.
#   clients/macos/check-notarizable.sh <path/to/Brook.app>
set -euo pipefail
app="${1:?usage: check-notarizable.sh <Brook.app>}"
[[ -d "$app" ]] || { echo "check-notarizable: no such app: $app" >&2; exit 1; }
failed=0
checked=0
# Every regular file, whatever its mode or name: `file` says what's a Mach-O (a resource or a
# nested binary needn't be executable or end in .dylib).
while IFS= read -r -d '' f; do
  file -b "$f" | grep -q "Mach-O" || continue
  checked=$((checked + 1))
  # Captured first: under pipefail, `grep -q` quitting early would fail codesign by SIGPIPE.
  info="$(codesign -dvv "$f" 2>&1)" || { echo "unsigned: ${f#"$app"/}" >&2; failed=1; continue; }
  problems=()
  grep -q '^Authority=Developer ID Application' <<<"$info" || problems+=("not signed by a Developer ID")
  grep -q 'flags=.*runtime' <<<"$info" || problems+=("no hardened runtime")
  grep -q '^Timestamp=' <<<"$info" || problems+=("no secure timestamp")
  # Captured, then tested: a pipe into `grep -q` can fail by SIGPIPE under pipefail and read as
  # "not there". An entitlements blob that won't parse is a problem too, not a pass.
  raw="$(codesign -d --entitlements - --xml "$f" 2>/dev/null)" || raw=""
  if [[ -n "$raw" ]]; then
    if ents="$(plutil -p - <<<"$raw" 2>/dev/null)"; then
      grep -q get-task-allow <<<"$ents" && problems+=("has get-task-allow")
    else
      problems+=("unreadable entitlements")
    fi
  fi
  if ((${#problems[@]})); then
    echo "${f#"$app"/}: $(IFS=,; echo "${problems[*]}")" >&2
    failed=1
  fi
done < <(find "$app" -type f -print0)
((checked > 0)) || { echo "check-notarizable: no binaries found in $app" >&2; exit 1; }
((failed == 0)) || { echo "check-notarizable: NOT notarizable" >&2; exit 1; }
echo "check-notarizable: ok ($checked binaries)"
