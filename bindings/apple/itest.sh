#!/usr/bin/env bash
# Integration tests of the Swift bindings against the shared test server (a real Brook
# stack on the Linux VM, operated by the server side). No local server is started here.
#
# Reads bindings/apple/.itest.env (gitignored, mode 600, filled in by a human):
#   BROOK_TEST_SERVER=http://host:port           (base URL, no /api/v1)
#   BROOK_TEST_HANDLE=mac                        (the shared dev account on the LAN test server)
#   BROOK_TEST_PASSWORD=...
#   BROOK_TEST_ALLOW_INSECURE_HTTP=1             (only if the server is plain http)
#   BROOK_TEST_CHANNEL=<channel uuid>            (a channel the account belongs to, for calls)
#   BROOK_TEST_ADMIN_HANDLE=admin                (registers throwaway accounts for the password tests)
#   BROOK_TEST_ADMIN_PASSWORD=...
# Optional, from the shell: BROOK_TEST_EXPECT_PEER=1 when a second participant publishes in that
# channel (the live acceptance); its media must then decode here.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ENV_FILE="$HERE/.itest.env"
# Every integration suite that must run, with its test count. A suite that is skipped or
# runs fewer tests fails the whole run.
SUITES=("LoginIntegrationTests:2" "CallRoundTripTests:2" "LiveCallTests:1" "PasswordChangeIntegrationTests:3" "SignOutIntegrationTests:1" "TotpIntegrationTests:1")

[[ -f "$ENV_FILE" ]] || { echo "missing $ENV_FILE (see header of $0)" >&2; exit 1; }
# `stat -f '%Lp'` is the BSD/macOS form (GNU stat uses `-c '%a'`); this script runs on the Mac.
if [[ "$(stat -f '%Lp' "$ENV_FILE")" != "600" ]]; then
  echo "$ENV_FILE must be mode 600 (it holds a password): chmod 600 $ENV_FILE" >&2; exit 1
fi
set -a; source "$ENV_FILE"; set +a
for v in BROOK_TEST_SERVER BROOK_TEST_HANDLE BROOK_TEST_PASSWORD BROOK_TEST_CHANNEL \
         BROOK_TEST_ADMIN_HANDLE BROOK_TEST_ADMIN_PASSWORD; do
  [[ -n "${!v:-}" ]] || { echo "$v is not set in $ENV_FILE" >&2; exit 1; }
done

health="${BROOK_TEST_SERVER%/}/health"
curl -fsS --max-time 10 "$health" >/dev/null || { echo "server not reachable: $health" >&2; exit 1; }

"$HERE/build-xcframework.sh"

cd "$HERE/swift/BrookCore"
log="$(mktemp)"; trap 'rm -f "$log"' EXIT
set +e
BROOK_REQUIRE_ITEST=1 swift test 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
set -e

# A green `swift test` is not enough: the integration tests must actually have run.
# XCTest prints the suite summary on the line after "Test Suite '<name>' passed|failed".
if (( status != 0 )); then echo "FAIL: swift test exited $status" >&2; exit "$status"; fi
for entry in "${SUITES[@]}"; do
  suite="${entry%%:*}"; want="${entry##*:}"
  line="$(awk "/Test Suite '$suite' (passed|failed)/{getline; print; exit}" "$log")"
  ran="$(sed -nE 's/.*Executed ([0-9]+) tests?.*/\1/p' <<<"$line")"
  skipped="$(sed -nE 's/.* ([0-9]+) tests? skipped.*/\1/p' <<<"$line")"
  if [[ "${ran:-0}" != "$want" || "${skipped:-0}" != "0" ]]; then
    echo "FAIL: $suite: expected $want run, 0 skipped; got ran=${ran:-0} skipped=${skipped:-0}" >&2
    exit 1
  fi
done
echo "PASS: ${SUITES[*]} ran against $BROOK_TEST_SERVER"
