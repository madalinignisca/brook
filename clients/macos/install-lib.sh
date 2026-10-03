#!/usr/bin/env bash
# The `install` mode of build.sh and the one-time move of the build directory, as functions so
# test-install.sh can run them against fake tools and no build. Sourced, never run.

# Spotlight skips folders named *.noindex: the build output must not offer a second Brook.app.
# An old `build` is moved (keeping the compiled cache); if both exist nothing is deleted.
migrate_build_dir() {
  local here="$1"
  if [[ -d "$here/build" && ! -e "$here/build.noindex" ]]; then
    mv "$here/build" "$here/build.noindex"
    # SwiftPM records the artifacts' absolute paths; left pointing at the old directory the next
    # build fails with "no XCFramework found".
    sed -i '' "s#${here}/build/SourcePackages#${here}/build.noindex/SourcePackages#g" \
      "$here/build.noindex/SourcePackages/workspace-state.json" 2>/dev/null || true
    echo "moved clients/macos/build to build.noindex (Spotlight skips it; the compiled cache is kept)"
  elif [[ -d "$here/build" && -d "$here/build.noindex" ]]; then
    echo "the old clients/macos/build folder is no longer used and can be deleted (Spotlight indexes it)"
  fi
  return 0
}

# Install the built app $1 to ${BROOK_INSTALL_DIR:-/Applications}/Brook.app. Never kills anything:
# a running Brook is a refusal. The copy goes to a temp name beside the target first, so a failed
# copy leaves the old app untouched.
install_app() {
  local app="$1" dir="${BROOK_INSTALL_DIR:-/Applications}"
  local target="$dir/Brook.app" tmp="$dir/.Brook.app.installing"
  if pgrep -x Brook >/dev/null 2>&1; then
    echo "Brook is running: quit it, then run install again (nothing was changed)" >&2
    return 1
  fi
  rm -rf "$tmp"
  if ! ditto "$app" "$tmp"; then
    rm -rf "$tmp"
    echo "copying the app failed; the installed app (if any) is unchanged" >&2
    return 1
  fi
  rm -rf "$target"
  mv "$tmp" "$target" || { echo "couldn't put the new app in place at $target" >&2; return 1; }
  echo "installed: $target"
  if codesign --verify --strict "$target"; then
    echo "codesign --verify --strict: ok"
  else
    echo "codesign --verify --strict FAILED for $target" >&2
    return 1
  fi
  echo "Spotlight will find this one app (the build folder is .noindex)."
}
