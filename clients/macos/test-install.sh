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
DITTO_CALLS=0; DITTO_HOOK="" # DITTO_HOOK runs once, right after a copy (an install overlapping this one)
ditto() { # a failing one leaves half a copy behind, like a full disk would
  DITTO_CALLS=$((DITTO_CALLS + 1))
  if [[ "$DITTO_FAILS" == 1 ]]; then mkdir -p "$2"; echo partial > "$2/partial"; return 1; fi
  command ditto "$@" || return 1
  if [[ -n "$DITTO_HOOK" ]]; then local h="$DITTO_HOOK"; DITTO_HOOK=""; eval "$h"; fi
}
fresh() { # a built app (version new) and an installed one (version old)
  PGREP_AFTER=-1; PGREP_CALLS=0; MV_FAIL=""; MV_FAIL_BACK=0; RM_FAIL=""; CODESIGN_RC=0; PGREP_RC=1; DITTO_FAILS=0; DITTO_HOOK=""
  rm -rf "$T/src" "$T/dest"
  mkdir -p "$T/src/Brook.app/Contents" "$T/dest/Brook.app/Contents"
  echo new > "$T/src/Brook.app/Contents/version"; echo old > "$T/dest/Brook.app/Contents/version"
  export BROOK_INSTALL_DIR="$T/dest"
}
nolock() { [[ ! -e "$T/dest/.Brook.app.lock" ]] && ok || bad "$1 releases the lock"; }
installed() { cat "$T/dest/Brook.app/Contents/version" 2>/dev/null; }

# ---- install ----------------------------------------------------------------
fresh; PGREP_RC=0; DITTO_FAILS=0
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a running Brook refuses" 1 "$rc"; has "it says why" "$out" "Brook is running"
check "a refusal leaves the old app" old "$(installed)"
[[ ! -e "$T/dest/.Brook.app.installing" ]] && ok || bad "a refusal leaves no temp copy"
nolock "a running-Brook refusal"

fresh; PGREP_RC=1; DITTO_FAILS=1
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a failed copy fails" 1 "$rc"
check "a failed copy leaves the old app intact" old "$(installed)"
[[ ! -e "$T/dest/.Brook.app.installing" ]] && ok || bad "a failed copy cleans its temp copy"
nolock "a failed copy"

fresh; DITTO_FAILS=0; CODESIGN_RC=0
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "success" 0 "$rc"; check "success replaces the app" new "$(installed)"
has "it prints the path" "$out" "installed: $T/dest/Brook.app"; has "it prints the verify result" "$out" "codesign --verify --deep --strict: ok"
has "it mentions Spotlight" "$out" "Spotlight"
has "it limits the Spotlight claim to build.sh builds" "$out" "builds made with build.sh"
[[ ! -e "$T/dest/.Brook.app.old" ]] && ok || bad "success leaves no backup"
[[ ! -e "$T/dest/.Brook.app.installing" ]] && ok || bad "success leaves no temp copy"
nolock "success"

fresh; CODESIGN_RC=1
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a failed verify of the staged copy fails" 1 "$rc"; has "and says so" "$out" "FAILED"
check "a failed stage verify leaves the old app untouched" old "$(installed)"
[[ ! -e "$T/dest/.Brook.app.installing" && ! -e "$T/dest/.Brook.app.old" ]] && ok || bad "a failed stage verify leaves no temp copy or backup"
nolock "a failed stage verify"

fresh; MV_FAIL=".Brook.app.installing"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a failed second mv fails" 1 "$rc"; has "and says so" "$out" "couldn't put the new app in place"
check "a failed second mv restores the old app" old "$(installed)"
has "and says it was put back" "$out" "put back"
[[ ! -e "$T/dest/.Brook.app.installing" && ! -e "$T/dest/.Brook.app.old" ]] && ok || bad "a failed second mv leaves no temp copy or backup"
nolock "a failed second mv"

fresh; MV_FAIL=".Brook.app.installing"; MV_FAIL_BACK=1
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a failed second mv and restore fails" 1 "$rc"; has "and names where the old app is" "$out" "$T/dest/.Brook.app.old"
[[ -f "$T/dest/.Brook.app.old/Contents/version" ]] && ok || bad "the old app is kept at the backup path"
has "and names the staged copy" "$out" "$T/dest/.Brook.app.installing"
[[ -f "$T/dest/.Brook.app.installing/Contents/version" ]] && ok || bad "the verified staged copy is kept"
nolock "a failed second mv and restore"

fresh; MV_FAIL="Brook.app"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a failed move-aside fails" 1 "$rc"; check "a failed move-aside leaves the old app" old "$(installed)"
[[ ! -e "$T/dest/.Brook.app.installing" ]] && ok || bad "a failed move-aside removes the staged copy"
nolock "a failed move-aside"

fresh; RM_FAIL=".Brook.app.old"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a failed backup removal still succeeds" 0 "$rc"; check "and the new app is in place" new "$(installed)"
has "and warns" "$out" "warning: couldn't remove the previous app"
nolock "a failed backup removal"

fresh; mkdir -p "$T/dest/.Brook.app.old"; echo stale > "$T/dest/.Brook.app.old/s"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a stale backup does not stop an install" 0 "$rc"; check "and the new app is in place" new "$(installed)"
[[ ! -e "$T/dest/Brook.app/.Brook.app.old" && ! -e "$T/dest/.Brook.app.old" ]] && ok || bad "no nested or stale backup remains"

# Nesting would make the restore put the stale backup (not the old app) back.
fresh; mkdir -p "$T/dest/.Brook.app.old"; echo stale > "$T/dest/.Brook.app.old/s"; MV_FAIL=".Brook.app.installing"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a stale backup is cleared, not nested into: the failed swap fails" 1 "$rc"
check "and the restore brings back the old app, not the stale backup" old "$(installed)"
nolock "a stale backup with a failed swap"

fresh; mkdir -p "$T/dest/.Brook.app.old"; RM_FAIL=".Brook.app.old"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "an unremovable stale backup refuses" 1 "$rc"; check "and leaves the old app" old "$(installed)"
[[ ! -e "$T/dest/.Brook.app.installing" ]] && ok || bad "an unremovable stale backup removes the staged copy"
nolock "an unremovable stale backup"

fresh; PGREP_AFTER=1 # the first check passes, the recheck sees a Brook
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a Brook started during the copy refuses" 1 "$rc"; has "and says why" "$out" "Brook is running"
check "the recheck leaves the old app intact" old "$(installed)"
[[ ! -e "$T/dest/.Brook.app.installing" && ! -e "$T/dest/.Brook.app.old" ]] && ok || bad "the recheck removes the staged copy"
nolock "the recheck refusal"

# (2) the staging area cannot be cleared
fresh; RM_FAIL=".Brook.app.installing"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "an uncleared staging path fails" 1 "$rc"; has "and says so" "$out" "couldn't clear $T/dest/.Brook.app.installing"
check "and leaves the old app" old "$(installed)"
nolock "an uncleared staging path"

# ---- the per-destination lock --------------------------------------------------
fresh; mkdir -p "$T/dest/.Brook.app.lock"; echo "$$" > "$T/dest/.Brook.app.lock/pid"; mkdir "$T/dest/.Brook.app.installing"; echo mine > "$T/dest/.Brook.app.installing/x"
DITTO_CALLS=0
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a live lock refuses" 1 "$rc"; has "and names the pid" "$out" "another install is running (pid $$)"
check "a refused install leaves the old app" old "$(installed)"
[[ -f "$T/dest/.Brook.app.installing/x" ]] && ok || bad "a refused install does not touch the other's staged copy"
check "a refused install copies nothing" 0 "$DITTO_CALLS"
[[ "$(cat "$T/dest/.Brook.app.lock/pid")" == "$$" ]] && ok || bad "a refused install leaves the other's lock"

fresh; mkdir -p "$T/dest/.Brook.app.lock"; : > "$T/dest/.Brook.app.lock/pid"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a lock with no pid yet counts as live" 1 "$rc"; check "and the old app stays" old "$(installed)"

fresh; sleep 0 & dead=$!; wait "$dead"
mkdir -p "$T/dest/.Brook.app.lock"; echo "$dead" > "$T/dest/.Brook.app.lock/pid"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "a lock of a dead pid is taken over" 0 "$rc"; has "and the takeover is announced" "$out" "taking over the lock"
check "and the install happens" new "$(installed)"
nolock "a takeover"

fresh
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "an install after a released lock" 0 "$rc"; nolock "the first of two installs"
echo old > "$T/src/Brook.app/Contents/version2"
out="$(install_app "$T/src/Brook.app" 2>&1)"; rc=$?
check "and a second one right after succeeds" 0 "$rc"

# A second install starting between the first one's stage and swap.
fresh
# B runs inside A's copy step (a subshell), so what B saw goes to a file.
DITTO_HOOK='{ install_app "$T/src/Brook.app" 2>&1; echo "rc=$?"; echo "staged=$(cat "$T/dest/.Brook.app.installing/Contents/version" 2>/dev/null)"; } > "$T/b.out"'
out="$(install_app "$T/src/Brook.app" 2>&1)"; outB="$(cat "$T/b.out")"
has "the overlapping install refuses" "$outB" "rc=1"; has "and names the running one" "$outB" "another install is running"
has "and the staged copy it saw is the first one's, complete" "$outB" "staged=new"
has "the first install completes" "$out" "installed: $T/dest/Brook.app"
check "and leaves a complete app" new "$(installed)"; [[ -f "$T/dest/Brook.app/Contents/version" ]] && ok || bad "the app is complete"
[[ ! -e "$T/dest/.Brook.app.installing" && ! -e "$T/dest/.Brook.app.old" ]] && ok || bad "no staged copy or backup remains after the overlap"
nolock "the overlap"

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
rm -rf "$W"; mkdir -p "$W/build/SourcePackages"; printf '{"a": "\\ud800"}' > "$W/build/SourcePackages/workspace-state.json"
out="$(migrate_build_dir "$W" 2>&1)"; rc=$?
check "a rewrite that cannot be written fails" 1 "$rc"
[[ ! -e "$W/build.noindex/SourcePackages/workspace-state.json.rewriting" ]] && ok || bad "a failed write leaves no temp file"
check "and the file is as it was" '{"a": "\ud800"}' "$(cat "$W/build.noindex/SourcePackages/workspace-state.json")"
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
