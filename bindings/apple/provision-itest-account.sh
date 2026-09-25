#!/usr/bin/env bash
# Create the integration-test account on a Brook server and write bindings/apple/.itest.env.
#
# Run it yourself, as the server's admin. Your admin password is read without echo and
# used for one login; the test account's password is generated here and written only to
# .itest.env (mode 600). Neither is printed, logged, or passed on the command line.
#
#   bindings/apple/provision-itest-account.sh [server]            # default: the shared LAN test server
#   bindings/apple/provision-itest-account.sh --bootstrap [server] # fresh server: register the admin first
#
# --bootstrap registers YOUR admin account first: on a fresh server the first account to
# register becomes the global admin. Use it once, only on an empty server.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ENV_FILE="$HERE/.itest.env"
HANDLE="itest-mac"

bootstrap=0
if [[ "${1:-}" == "--bootstrap" ]]; then bootstrap=1; shift; fi
SERVER="${1:-http://192.168.1.192:8080}"
SERVER="${SERVER%/}"
API="$SERVER/api/v1"

command -v curl >/dev/null && command -v python3 >/dev/null || { echo "needs curl and python3" >&2; exit 1; }
[[ -e "$ENV_FILE" ]] && { echo "$ENV_FILE already exists; remove it first to re-provision" >&2; exit 1; }
curl -fsS --max-time 10 "$SERVER/health" >/dev/null || { echo "server not reachable: $SERVER/health" >&2; exit 1; }

# JSON bodies are built by python from environment variables, so no secret ever appears
# in a process argument list (visible to other users via `ps`).
json() { python3 -c 'import json,os,sys; print(json.dumps({k: os.environ[k] for k in sys.argv[1:]}))' "$@"; }
# The bearer token goes through a private temp file (curl's `-H @file`), not argv.
HDR="$(umask 077; mktemp)"; trap 'rm -f "$HDR"' EXIT
post() { # url [bearer] ; body on stdin; prints body then the HTTP status on the last line
  local url="$1" auth=()
  if [[ -n "${2:-}" ]]; then printf 'Authorization: Bearer %s\n' "$2" > "$HDR"; auth=(-H "@$HDR"); fi
  curl -sS --max-time 15 -w '\n%{http_code}' -X POST -H 'Content-Type: application/json' \
    "${auth[@]}" --data-binary @- "$url"
}
check() { # response-with-status-line expected-status what
  local body status; status="$(tail -n1 <<<"$1")"; body="$(sed '$d' <<<"$1")"
  [[ "$status" == "$2" ]] || { echo "$3 failed (HTTP $status): $body" >&2; exit 1; }
  printf '%s' "$body"
}

read -rp "Admin handle: " handle
read -rsp "Admin password: " password; echo
export handle password

if (( bootstrap )); then
  read -rp "Admin display name: " display_name; export display_name
  check "$(json handle display_name password | post "$API/auth/register")" 201 "admin registration" >/dev/null
  echo "admin '$handle' registered"
fi

token="$(check "$(json handle password | post "$API/auth/login")" 200 "admin login" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["access_token"])')"
unset password

export handle="$HANDLE" display_name="Mac integration tests"
export password="$(python3 -c 'import secrets; print(secrets.token_urlsafe(24))')"
check "$(json handle display_name password | post "$API/auth/register" "$token")" 201 "creating $HANDLE" >/dev/null

insecure=""; [[ "$SERVER" == http://* ]] && insecure="BROOK_TEST_ALLOW_INSECURE_HTTP=1"
( umask 077
  printf 'BROOK_TEST_SERVER=%s\nBROOK_TEST_HANDLE=%s\nBROOK_TEST_PASSWORD=%s\n%s\n' \
    "$SERVER" "$HANDLE" "$password" "$insecure" > "$ENV_FILE" )
unset password token
echo "created '$HANDLE' and wrote $ENV_FILE (mode $(stat -f '%Lp' "$ENV_FILE")). Now run: bindings/apple/itest.sh"
