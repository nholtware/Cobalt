#!/usr/bin/env bash
# Mint, refresh, install and check the Inoreader token. Secrets live only in
# 0600 files under $S and are never passed as command-line arguments.
set -euo pipefail
umask 077

S="${KOBO_SECRETS_DIR:-$HOME/.config/cobalt/secrets}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
REDIRECT="https://127.0.0.1/inoreader-callback"
TOKEN_URL="https://www.inoreader.com/oauth2/token"

need() { # need FILE SUBCOMMAND-THAT-CREATES-IT
  [ -s "$S/$1" ] || { echo "missing $S/$1: create it with '$2'" >&2; exit 1; }
}

extract() { # reads $S/inoreader-token.json, writes the token files
  python3 - "$S" <<'PY'
import json, os, sys, time
s = sys.argv[1]
d = json.load(open(os.path.join(s, "inoreader-token.json")))
def put(name, value):
    p = os.path.join(s, name)
    fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(value + "\n")
    os.chmod(p, 0o600)
put("inoreader", d["access_token"])
if d.get("refresh_token"):
    put("inoreader-refresh", d["refresh_token"])
else:
    print("reply had no refresh_token: kept the old inoreader-refresh file")
print("token_type:", d.get("token_type"))
print("expires_in:", d.get("expires_in"))
print("scope:", d.get("scope"))
print("minted:", time.strftime("%Y-%m-%dT%H:%M"))
PY
}

post_token() { # post_token BODYFILE
  local status
  status="$(curl -sS --data @"$1" -H 'Content-Type: application/x-www-form-urlencoded' \
    -o "$S/inoreader-token.json" -w '%{http_code}' "$TOKEN_URL")" || status=000
  if [ "$status" != 200 ]; then
    echo "token endpoint answered HTTP $status" >&2
    python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));print("error:",d.get("error"),d.get("error_description",""),file=sys.stderr)' \
      "$S/inoreader-token.json" 2>&1 || true
    rm -f "$1" "$S/inoreader-token.json"
    exit 1
  fi
  extract
  rm -f "$1" "$S/inoreader-token.json"
}

form() { # form BODYFILE PYTHON-DICT-EXPRESSION over the files in $S
  python3 - "$S" "$1" "$2" <<'PY'
import os, sys, urllib.parse
s, out, kind = sys.argv[1:4]
r = lambda n: open(os.path.join(s, n)).read().strip()
if kind == "mint":
    d = {"code": r("inoreader-code"), "redirect_uri": "https://127.0.0.1/inoreader-callback",
         "client_id": r("inoreader-client-id"), "client_secret": r("inoreader-client-secret"),
         "scope": "", "grant_type": "authorization_code"}
else:
    d = {"client_id": r("inoreader-client-id"), "client_secret": r("inoreader-client-secret"),
         "grant_type": "refresh_token", "refresh_token": r("inoreader-refresh")}
fd = os.open(out, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as f:
    f.write(urllib.parse.urlencode(d))
PY
}

cmd_url() {
  need inoreader-client-id "create an application in Inoreader (Preferences, Developer)"
  local id
  id="$(cat "$S/inoreader-client-id")"
  printf 'https://www.inoreader.com/oauth2/auth?client_id=%s&redirect_uri=%s&response_type=code&scope=read%%20write&state=cobalt\n' \
    "$id" "$REDIRECT"
  echo
  echo "Open it, approve, then copy only the code= value from the address bar"
  echo "(the page itself will not load) into $S/inoreader-code as one line."
  echo "Then run: token.sh mint"
}

cmd_mint() {
  if [ "${1:-}" = "--wait" ]; then # codes expire fast: mint the moment the file appears
    local i
    for i in $(seq 1 600); do
      [ -s "$S/inoreader-code" ] && break
      sleep 0.5
    done
    sleep 0.3
  fi
  need inoreader-code "token.sh url"
  need inoreader-client-id "create an application in Inoreader"
  need inoreader-client-secret "create an application in Inoreader"
  local body="$S/inoreader-body"
  form "$body" mint
  post_token "$body"
  rm -f "$S/inoreader-code"
}

cmd_refresh() {
  need inoreader-refresh "token.sh mint"
  need inoreader-client-id "create an application in Inoreader"
  need inoreader-client-secret "create an application in Inoreader"
  local body="$S/inoreader-body"
  form "$body" refresh
  post_token "$body"
}

cmd_install() {
  case "${1:-}" in
    --device)
      [ -n "${2:-}" ] || { echo "usage: token.sh install --device <ip>" >&2; exit 2; }
      cd "$ROOT" && cargo run -q -p kobo-cli -- secret set inoreader --device "$2" ;;
    --sim)
      need inoreader "token.sh mint"
      local dir="${TMPDIR:-/tmp}/cobalt-sim-secrets"
      mkdir -p "$dir"
      install -m 600 "$S/inoreader" "$dir/inoreader"
      echo "installed to $dir/inoreader"
      echo "start the simulator with the same TMPDIR=${TMPDIR:-/tmp}" ;;
    *) echo "usage: token.sh install --device <ip> | --sim" >&2; exit 2 ;;
  esac
}

cmd_check() {
  need inoreader "token.sh mint"
  local out status
  out="$(curl -sS -D - -o /dev/null -H "Authorization: Bearer $(cat "$S/inoreader")" \
    'https://www.inoreader.com/reader/api/0/user-info' | tr -d '\r')"
  printf '%s\n' "$out" | grep -i '^HTTP\|^x-reader' || true
  status="$(printf '%s\n' "$out" | awk 'toupper($1) ~ /^HTTP/ {print $2; exit}')"
  [ "$status" = 200 ]
}

case "${1:-}" in
  url) cmd_url ;;
  mint) shift; cmd_mint "$@" ;;
  refresh) cmd_refresh ;;
  install) shift; cmd_install "$@" ;;
  check) cmd_check ;;
  *) echo "usage: token.sh url|mint|refresh|install --device <ip>|install --sim|check" >&2; exit 2 ;;
esac
