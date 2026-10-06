#!/usr/bin/env bash
# Fails if app code reads a person's display name outside PersonName.swift (#238): every person
# must go through PersonName so Show usernames applies. A heuristic: it misses a name read through
# another property, so each surface has its own test too.
#   clients/macos/check-person-names.sh [source dir]
# A line that must read the raw name says why, in a `// raw name: <why>` comment (storing a
# (name, handle) pair, editing your own name, the argument of PersonName). `displayName(` is
# DropImport's, not a person's.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="${1:-$HERE/Brook}"
[[ -d "$SRC" ]] || { echo "check-person-names: no such directory: $SRC" >&2; exit 2; }
hits="$(grep -rnE --include='*.swift' '\.displayName|authorDisplayName' "$SRC" \
  | grep -v '/PersonName\.swift:' \
  | grep -vE 'displayName\(' \
  | grep -vE '// raw name: .+')"
if [[ -n "$hits" ]]; then
  echo "check-person-names: a person's display name is read outside PersonName (use PersonName.label," >&2
  echo "or mark the line '// raw name: <why>'):" >&2
  echo "$hits" >&2
  exit 1
fi
