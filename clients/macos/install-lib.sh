#!/usr/bin/env bash
# The `install` mode of build.sh and the one-time move of the build directory, as functions so
# test-install.sh can run them against fake tools and no build. Sourced, never run.

# Rewrite $1 (old prefix) to $2 (new prefix) in every string of the JSON file $3. The paths are
# arguments, never part of the program text or a pattern, so no character in them can break it;
# the result is checked as JSON and written by rename, so a failure leaves the file as it was.
rewrite_json_paths() {
  /usr/bin/python3 - "$1" "$2" "$3" <<'PY'
import json, os, sys
old, new, path = sys.argv[1:4]
def walk(v):
    if isinstance(v, str):
        return v.replace(old, new)
    if isinstance(v, list):
        return [walk(x) for x in v]
    if isinstance(v, dict):
        return {walk(k): walk(x) for k, x in v.items()}
    return v
with open(path, encoding="utf-8") as f:
    data = json.load(f)
text = json.dumps(walk(data), indent=2, ensure_ascii=False)
json.loads(text)
tmp = path + ".rewriting"
try:
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(text + "\n")
    os.replace(tmp, path)
except BaseException:
    # A failed write (a string that is not valid UTF-8, a full disk) must not leave the temp file.
    if os.path.exists(tmp):
        os.remove(tmp)
    raise
PY
}

# Spotlight skips folders named *.noindex: the build output must not offer a second Brook.app.
# An old `build` is moved (keeping the compiled cache); if both exist nothing is deleted.
migrate_build_dir() {
  local here="$1" state="$1/build.noindex/SourcePackages/workspace-state.json"
  if [[ -d "$here/build" && ! -e "$here/build.noindex" ]]; then
    mv "$here/build" "$here/build.noindex" || { echo "couldn't move $here/build to build.noindex" >&2; return 1; }
    # SwiftPM records the artifacts' absolute paths; left pointing at the old directory the next
    # build fails with "no XCFramework found". Never hidden: the directory has already moved.
    if [[ -f "$state" ]] && ! rewrite_json_paths "$here/build/SourcePackages" "$here/build.noindex/SourcePackages" "$state"; then
      echo "could not update SourcePackages paths; delete clients/macos/build.noindex/SourcePackages and build again" >&2
      return 1
    fi
    echo "moved clients/macos/build to build.noindex (Spotlight skips it; the compiled cache is kept)"
  elif [[ -d "$here/build" && -d "$here/build.noindex" ]]; then
    echo "the old clients/macos/build folder is no longer used and can be deleted (Spotlight indexes it)"
  fi
  return 0
}

# Never kill Brook: a running one is a refusal.
refuse_if_brook_running() {
  if pgrep -x Brook >/dev/null 2>&1; then
    echo "Brook is running: quit it, then run install again (nothing was changed)" >&2
    return 1
  fi
  return 0
}

# One install at a time per destination: two would share the staged copy and the backup, and one
# could install the other's half-written copy and delete the working backup. The lock is a
# directory (mkdir is atomic) holding the pid of its owner. A lock whose owner is gone is taken
# over; a live one is a refusal that touches nothing. Runs in install_app's own subshell, so
# $BASHPID is the owner and the traps below are the subshell's: build.sh's traps are not touched.
_install_lock_acquire() {
  local lock="$1" pid
  mkdir -p "$(dirname "$lock")" 2>/dev/null
  if ! mkdir "$lock" 2>/dev/null; then
    pid="$(cat "$lock/pid" 2>/dev/null)"
    # No pid yet: its owner is between mkdir and the write, so it is live.
    if [[ -z "$pid" ]] || kill -0 "$pid" 2>/dev/null; then
      echo "another install is running (pid ${pid:-unknown}); nothing was changed (a stale $lock can be deleted by hand)" >&2
      return 1
    fi
    echo "taking over the lock of the install that died (pid $pid)" >&2
    rm -rf "$lock" || { echo "couldn't clear the stale lock $lock" >&2; return 1; }
    mkdir "$lock" 2>/dev/null || { echo "another install took the lock first; nothing was changed" >&2; return 1; }
  fi
  echo "$BASHPID" > "$lock/pid"
}

# Only the owner's lock is removed.
_install_lock_release() {
  if [[ "$(cat "$1/pid" 2>/dev/null)" == "$BASHPID" ]]; then rm -rf "$1"; fi
  return 0
}

# Install the built app $1 to ${BROOK_INSTALL_DIR:-/Applications}/Brook.app. The copy is staged and
# verified beside the target first; the old app is moved aside (not deleted) until the new one is
# in place, so no failure leaves the user without an app. Called as `install_app ... || exit 1`,
# which turns `set -e` off here: every command checks its own status. A subshell, so the lock is
# released on every way out (return, INT, TERM) without replacing the caller's traps.
install_app() (
  local lock="${BROOK_INSTALL_DIR:-/Applications}/.Brook.app.lock"
  trap '_install_lock_release "$lock"' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  _install_lock_acquire "$lock" || exit 1
  _install_app_locked "$@"
)

_install_app_locked() {
  local app="$1" dir="${BROOK_INSTALL_DIR:-/Applications}"
  local target="$dir/Brook.app" tmp="$dir/.Brook.app.installing" old="$dir/.Brook.app.old"
  refuse_if_brook_running || return 1
  rm -rf "$tmp" || { echo "couldn't clear $tmp; nothing was changed" >&2; return 1; }
  if ! ditto "$app" "$tmp"; then
    rm -rf "$tmp"
    echo "copying the app failed; the installed app (if any) is unchanged" >&2
    return 1
  fi
  # The staged copy, same strictness as check-decoder.sh, before the target is touched.
  if ! codesign --verify --deep --strict "$tmp"; then
    rm -rf "$tmp"
    echo "codesign --verify --deep --strict FAILED for the new copy; the installed app (if any) is unchanged" >&2
    return 1
  fi
  # The copy and verify take a while: Brook may have been started meanwhile.
  if ! refuse_if_brook_running; then
    rm -rf "$tmp"
    return 1
  fi
  if [[ -e "$target" ]]; then
    # A backup left by an earlier failed run would make the mv nest the app inside it.
    if [[ -e "$old" ]] && ! rm -rf "$old"; then
      rm -rf "$tmp"
      echo "couldn't clear the stale $old; the installed app is unchanged" >&2
      return 1
    fi
    if ! mv "$target" "$old"; then
      rm -rf "$tmp"
      echo "couldn't move the installed app aside; it is unchanged" >&2
      return 1
    fi
  fi
  if ! mv "$tmp" "$target"; then
    echo "couldn't put the new app in place at $target" >&2
    if [[ -e "$old" ]]; then
      if mv "$old" "$target"; then
        echo "the previous app was put back" >&2
      else
        # Both ways failed: the verified staged copy is a second way back, so it stays.
        echo "the previous app is at $old and the new, verified copy is at $tmp (put one in place by hand)" >&2
        return 1
      fi
    fi
    rm -rf "$tmp"
    return 1
  fi
  rm -rf "$old" || echo "warning: couldn't remove the previous app at $old; delete it by hand" >&2
  echo "installed: $target"
  echo "codesign --verify --deep --strict: ok"
  echo "Spotlight will find this one app for builds made with build.sh (the build folder is .noindex)."
}
