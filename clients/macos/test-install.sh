#!/usr/bin/env bash
# Tests for install-lib.sh (build.sh install, and the build.noindex move), run by hand: no build,
# nothing outside a temp directory is touched.
#   clients/macos/test-install.sh
# Fake `pgrep`, `ditto` and `codesign` are shell functions shadowing the real tools.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
source "$HERE/install-lib.sh"
pass=0; fail=0
ok()   { pass=$((pass + 1)); }
bad()  { fail=$((fail + 1)); echo "FAIL: $1" >&2; }
check() { if [[ "$2" == "$3" ]]; then ok; else bad "$1 (expected $2, got $3)"; fi; }
has()   { if [[ "$2" == *"$3"* ]]; then ok; else bad "$1 (no '$3' in: ${2:0:200})"; fi; }
lacks() { if [[ "$2" != *"$3"* ]]; then ok; else bad "$1 (unexpected '$3')"; fi; }
T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT

PGREP_RC=1; DITTO_FAILS=0; CODESIGN_RC=0
pgrep() { return "$PGREP_RC"; }
codesign() { return "$CODESIGN_RC"; }
ditto() { # a failing one leaves half a copy behind, like a full disk would
  if [[ "$DITTO_FAILS" == 1 ]]; then mkdir -p "$2"; echo partial > "$2/partial"; return 1; fi
  command ditto "$@"
}
fresh() { # a built app (version new) and an installed one (version old)
  rm -rf "$T/src" "$T/dest"
  mkdir -p "$T/src/Brook.app/Contents" "$T/dest/Brook.app/Contents"
  echo new > "$T/src/Brook.app/Contents/version"; echo old > "$T/dest/Brook.app/Contents/version"
  export BROOK_INSTALL_DIR="$T/dest"
}
installed() { cat "$T/dest/Brook.app/Contents/version" 2>/dev/null; }

# ---- install ----------------------------------------------------------------
fresh; PGREP_RC=0; DITTO_FAILS=0
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a running Brook refuses" 1 "$rc"; has "it says why" "$out" "Brook is running"
check "a refusal leaves the old app" old "$(installed)"
[[ ! -e "$T/dest/.Brook.app.installing" ]] && ok || bad "a refusal leaves no temp copy"

fresh; PGREP_RC=1; DITTO_FAILS=1
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a failed copy fails" 1 "$rc"
check "a failed copy leaves the old app intact" old "$(installed)"
[[ ! -e "$T/dest/.Brook.app.installing" ]] && ok || bad "a failed copy cleans its temp copy"

fresh; DITTO_FAILS=0; CODESIGN_RC=0
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "success" 0 "$rc"; check "success replaces the app" new "$(installed)"
has "it prints the path" "$out" "installed: $T/dest/Brook.app"; has "it prints the verify result" "$out" "codesign --verify --strict: ok"
has "it mentions Spotlight" "$out" "Spotlight"
[[ ! -e "$T/dest/.Brook.app.installing" ]] && ok || bad "success leaves no temp copy"

fresh; CODESIGN_RC=1
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a failed signature check fails" 1 "$rc"; has "and says so" "$out" "FAILED"

rm -rf "$T/dest"; mkdir -p "$T/dest"; export BROOK_INSTALL_DIR="$T/dest"; CODESIGN_RC=0
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a first install (nothing to replace)" 0 "$rc"; check "it installs" new "$(installed)"

# ---- the build directory move -------------------------------------------------
rm -rf "$T/m"; mkdir -p "$T/m/build"; echo cache > "$T/m/build/c"
out="$(migrate_build_dir "$T/m" 2>&1)"
[[ -f "$T/m/build.noindex/c" && ! -e "$T/m/build" ]] && ok || bad "build moves to build.noindex, cache kept"
has "the move is announced" "$out" "moved"

rm -rf "$T/m"; mkdir -p "$T/m/build/SourcePackages"
echo "\"$T/m/build/SourcePackages/a\"" > "$T/m/build/SourcePackages/workspace-state.json"
migrate_build_dir "$T/m" >/dev/null
has "the moved package state points at the new directory" "$(cat "$T/m/build.noindex/SourcePackages/workspace-state.json")" "$T/m/build.noindex/SourcePackages/a"

rm -rf "$T/m"; mkdir -p "$T/m/build" "$T/m/build.noindex"; echo old > "$T/m/build/o"; echo new > "$T/m/build.noindex/n"
out="$(migrate_build_dir "$T/m" 2>&1)"
[[ -f "$T/m/build/o" && -f "$T/m/build.noindex/n" ]] && ok || bad "with both present nothing is moved or deleted"
has "both present: the delete hint" "$out" "can be deleted"; lacks "both present: no move" "$out" "moved"

rm -rf "$T/m"; mkdir -p "$T/m/build.noindex"
out="$(migrate_build_dir "$T/m" 2>&1)"; rc=$?
check "only build.noindex: quiet success" 0 "$rc"; check "and no output" "" "$out"
rm -rf "$T/m"; mkdir -p "$T/m"
out="$(migrate_build_dir "$T/m" 2>&1)"; rc=$?
check "neither: quiet success" 0 "$rc"; check "and no output" "" "$out"

echo "test-install: $pass passed, $fail failed"
[[ "$fail" -eq 0 ]]
