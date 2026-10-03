#!/usr/bin/env bash
# Tests for install-lib.sh (build.sh install, and the build.noindex move), run by hand: no build,
# nothing outside a temp directory is touched.
#   clients/macos/test-install.sh
# Fake `pgrep`, `ditto`, `codesign`, `mv` and `rm` are shell functions shadowing the real tools.
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
PGREP_AFTER=-1; PGREP_CALLS=0 # from call PGREP_AFTER+1 on, a Brook is running (-1: never)
MV_FAIL=""; MV_FAIL_BACK=0; RM_FAIL="" # operations whose first argument ends so fail
pgrep() {
  PGREP_CALLS=$((PGREP_CALLS + 1))
  if [[ "$PGREP_AFTER" -ge 0 && "$PGREP_CALLS" -gt "$PGREP_AFTER" ]]; then return 0; fi
  return "$PGREP_RC"
}
codesign() { return "$CODESIGN_RC"; }
mv() {
  if [[ -n "$MV_FAIL" && "$1" == *"$MV_FAIL" ]]; then return 1; fi
  if [[ "$MV_FAIL_BACK" == 1 && "$1" == *".Brook.app.old" ]]; then return 1; fi
  command mv "$@"
}
rm() {
  if [[ -n "$RM_FAIL" && "${*: -1}" == *"$RM_FAIL" ]]; then return 1; fi
  command rm "$@"
}
ditto() { # a failing one leaves half a copy behind, like a full disk would
  if [[ "$DITTO_FAILS" == 1 ]]; then mkdir -p "$2"; echo partial > "$2/partial"; return 1; fi
  command ditto "$@"
}
fresh() { # a built app (version new) and an installed one (version old)
  PGREP_AFTER=-1; PGREP_CALLS=0; MV_FAIL=""; MV_FAIL_BACK=0; RM_FAIL=""; CODESIGN_RC=0; PGREP_RC=1; DITTO_FAILS=0
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
has "it prints the path" "$out" "installed: $T/dest/Brook.app"; has "it prints the verify result" "$out" "codesign --verify --deep --strict: ok"
has "it mentions Spotlight" "$out" "Spotlight"
has "it limits the Spotlight claim to build.sh builds" "$out" "builds made with build.sh"
[[ ! -e "$T/dest/.Brook.app.old" ]] && ok || bad "success leaves no backup"
[[ ! -e "$T/dest/.Brook.app.installing" ]] && ok || bad "success leaves no temp copy"

fresh; CODESIGN_RC=1
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a failed verify of the staged copy fails" 1 "$rc"; has "and says so" "$out" "FAILED"
check "a failed stage verify leaves the old app untouched" old "$(installed)"
[[ ! -e "$T/dest/.Brook.app.installing" && ! -e "$T/dest/.Brook.app.old" ]] && ok || bad "a failed stage verify leaves no temp copy or backup"

fresh; MV_FAIL=".Brook.app.installing"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a failed second mv fails" 1 "$rc"; has "and says so" "$out" "couldn't put the new app in place"
check "a failed second mv restores the old app" old "$(installed)"
has "and says it was put back" "$out" "put back"
[[ ! -e "$T/dest/.Brook.app.installing" && ! -e "$T/dest/.Brook.app.old" ]] && ok || bad "a failed second mv leaves no temp copy or backup"

fresh; MV_FAIL=".Brook.app.installing"; MV_FAIL_BACK=1
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a failed second mv and restore fails" 1 "$rc"; has "and names where the old app is" "$out" "$T/dest/.Brook.app.old"
[[ -f "$T/dest/.Brook.app.old/Contents/version" ]] && ok || bad "the old app is kept at the backup path"

fresh; MV_FAIL="Brook.app"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a failed move-aside fails" 1 "$rc"; check "a failed move-aside leaves the old app" old "$(installed)"
[[ ! -e "$T/dest/.Brook.app.installing" ]] && ok || bad "a failed move-aside removes the staged copy"

fresh; RM_FAIL=".Brook.app.old"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a failed backup removal still succeeds" 0 "$rc"; check "and the new app is in place" new "$(installed)"
has "and warns" "$out" "warning: couldn't remove the previous app"

fresh; mkdir -p "$T/dest/.Brook.app.old"; echo stale > "$T/dest/.Brook.app.old/s"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a stale backup is cleared, not nested into" 0 "$rc"; check "and the new app is in place" new "$(installed)"
[[ ! -e "$T/dest/Brook.app/.Brook.app.old" && ! -e "$T/dest/.Brook.app.old" ]] && ok || bad "no nested or stale backup remains"

fresh; mkdir -p "$T/dest/.Brook.app.old"; RM_FAIL=".Brook.app.old"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "an unremovable stale backup refuses" 1 "$rc"; check "and leaves the old app" old "$(installed)"
[[ ! -e "$T/dest/.Brook.app.installing" ]] && ok || bad "an unremovable stale backup removes the staged copy"

fresh; PGREP_AFTER=1 # the first check passes, the recheck sees a Brook
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a Brook started during the copy refuses" 1 "$rc"; has "and says why" "$out" "Brook is running"
check "the recheck leaves the old app intact" old "$(installed)"
[[ ! -e "$T/dest/.Brook.app.installing" && ! -e "$T/dest/.Brook.app.old" ]] && ok || bad "the recheck removes the staged copy"

fresh; PGREP_RC=0
refuse_if_brook_running >/dev/null 2>&1; check "the early check refuses while Brook runs" 1 "$?"
PGREP_RC=1
refuse_if_brook_running >/dev/null 2>&1; check "the early check passes otherwise" 0 "$?"

rm -rf "$T/dest"; mkdir -p "$T/dest"; export BROOK_INSTALL_DIR="$T/dest"; CODESIGN_RC=0
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a first install (nothing to replace)" 0 "$rc"; check "it installs" new "$(installed)"

# ---- the build directory move -------------------------------------------------
rm -rf "$T/m"; mkdir -p "$T/m/build"; echo cache > "$T/m/build/c"
out="$(migrate_build_dir "$T/m" 2>&1)"
[[ -f "$T/m/build.noindex/c" && ! -e "$T/m/build" ]] && ok || bad "build moves to build.noindex, cache kept"
has "the move is announced" "$out" "moved"

# A checkout path full of characters that break sed patterns and replacements.
W="$T/we&ird #1 [B] 'q' \"d\" a.b*c"
rm -rf "$W"; mkdir -p "$W/build/SourcePackages"
python3 -c 'import json,sys; json.dump({"artifacts":[{"path":sys.argv[1]+"/build/SourcePackages/a"}],sys.argv[1]+"/build/SourcePackages/k":1,"other":"/x/build/SourcePackages/y"}, open(sys.argv[2],"w"))' "$W" "$W/build/SourcePackages/workspace-state.json"
out="$(migrate_build_dir "$W" 2>&1)"; rc=$?
check "a migration with special characters in the path succeeds" 0 "$rc"
res="$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["artifacts"][0]["path"]); print(list(d)[1]); print(d["other"])' "$W/build.noindex/SourcePackages/workspace-state.json" 2>&1)"
check "valid JSON, new prefix in values and keys, others untouched" "$W/build.noindex/SourcePackages/a
$W/build.noindex/SourcePackages/k
/x/build/SourcePackages/y" "$res"
lacks "no leftover old prefix" "$res" "$W/build/SourcePackages"
[[ ! -e "$W/build.noindex/SourcePackages/workspace-state.json.rewriting" ]] && ok || bad "no temp file left"

rm -rf "$W"; mkdir -p "$W/build/SourcePackages"; echo "not json {" > "$W/build/SourcePackages/workspace-state.json"
out="$(migrate_build_dir "$W" 2>&1)"; rc=$?
check "a failing rewrite fails" 1 "$rc"
has "and prints the hint" "$out" "could not update SourcePackages paths; delete clients/macos/build.noindex/SourcePackages and build again"
check "and leaves the file as it was" "not json {" "$(cat "$W/build.noindex/SourcePackages/workspace-state.json")"
rm -rf "$W"

rm -rf "$T/m"; mkdir -p "$T/m/build/SourcePackages"
echo '{}' > "$T/m/build/SourcePackages/workspace-state.json"
out="$(migrate_build_dir "$T/m" 2>&1)"; rc=$?
check "a plain migration succeeds" 0 "$rc"; has "and announces the move" "$out" "moved"

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
