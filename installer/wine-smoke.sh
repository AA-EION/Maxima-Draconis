#!/bin/bash
# Smoke-test MaximaSetup.exe in a throwaway Wine prefix — the in-bottle mode
# Draconis drives through CrossOver. No EA login is possible on CI, so this
# checks what can fail without one: the installed layout, the protocol
# handlers, that the Run-key autostart is skipped under Wine, and that the
# thin-client CLI really spawns maxima-server.exe (and that the server, waiting
# on a login that never comes, doesn't spin a core on its closed stdin).
#
# It then installs Maxima into a second prefix and checks that the two never
# talk to each other: Wine prefixes share the host loopback, so each server
# must be found through its own prefix's instance.json and must refuse the
# other prefix's token.
#
# Usage: WINEPREFIX=/tmp/prefix bash installer/wine-smoke.sh path/to/MaximaSetup.exe
set -euo pipefail

SETUP="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
: "${WINEPREFIX:?set WINEPREFIX to a scratch directory}"
export WINEPREFIX
WINE="${WINE:-$(command -v wine || command -v wine64)}"
WINESERVER="${WINESERVER:-$(command -v wineserver)}"

fail() { echo "✗ $*" >&2; exit 1; }
ok() { echo "✓ $*"; }

PREFIX2="${WINEPREFIX}-second"

cleanup() {
    "$WINESERVER" -k >/dev/null 2>&1 || true
    WINEPREFIX="$PREFIX2" "$WINESERVER" -k >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== Initialising prefix at $WINEPREFIX"
"$WINE" wineboot -i >/dev/null 2>&1 || true

echo "== Running MaximaSetup.exe /S"
"$WINE" "$SETUP" /S

INSTDIR=""
for d in "Program Files/Maxima" "Program Files (x86)/Maxima"; do
    if [[ -f "$WINEPREFIX/drive_c/$d/maxima-cli.exe" ]]; then
        INSTDIR="$WINEPREFIX/drive_c/$d"
        break
    fi
done
[[ -n "$INSTDIR" ]] || fail "maxima-cli.exe not found under drive_c/Program Files*/Maxima"
ok "installed to ${INSTDIR#"$WINEPREFIX/"}"

for exe in maxima-cli.exe maxima-server.exe maxima-bootstrap.exe maxima-service.exe; do
    [[ -f "$INSTDIR/$exe" ]] || fail "$exe missing from the install"
done
ok "core binaries present (cli, server, bootstrap, service)"

for proto in link2ea origin2 qrc; do
    "$WINE" reg query "HKCR\\$proto\\shell\\open\\command" 2>/dev/null \
        | grep -qi "maxima-bootstrap.exe" || fail "$proto:// is not routed to maxima-bootstrap.exe"
done
ok "link2ea:// origin2:// qrc:// registered"

if "$WINE" reg query 'HKCU\Software\Microsoft\Windows\CurrentVersion\Run' /v MaximaServer >/dev/null 2>&1; then
    fail "MaximaServer Run-key autostart was written inside a Wine prefix"
fi
ok "Run-key autostart skipped under Wine"

CLI="$INSTDIR/maxima-cli.exe"
status="$("$WINE" "$CLI" server-status --json 2>/dev/null | tr -d '\r' | grep '^{' || true)"
echo "   server-status: $status"
echo "$status" | grep -q '"running":false' || fail "server-status --json did not report a stopped server"
ok "server-status --json works without a server"

echo "== list-games --json (should spawn maxima-server.exe, which then waits for login)"
"$WINE" "$CLI" list-games --json >/dev/null 2>&1 &
cli_pid=$!

server_pid=""
for _ in $(seq 1 60); do
    server_pid="$(pgrep -f 'maxima-server\.exe' | head -n1 || true)"
    [[ -n "$server_pid" ]] && break
    sleep 1
done
[[ -n "$server_pid" ]] || fail "maxima-cli did not spawn maxima-server.exe within 60s"
ok "maxima-cli spawned maxima-server.exe (pid $server_pid)"

# Let the server settle into the OAuth wait, then sample its CPU. Before the
# stdin-EOF fix it pinned a full core here.
sleep 15
cpu="$(ps -o %cpu= -p "$server_pid" | tr -d ' ' || echo 0)"
echo "   maxima-server.exe CPU while waiting for login: ${cpu}%"
awk -v c="${cpu:-0}" 'BEGIN { exit (c < 50) ? 0 : 1 }' \
    || fail "maxima-server.exe is spinning (${cpu}% CPU) while waiting for login"
ok "server idles while waiting for login"

kill "$cli_pid" >/dev/null 2>&1 || true

# --- Two prefixes, two isolated servers -------------------------------------

instance_file() {
    find "$1/drive_c/users" -path '*ArmchairDevelopers/Maxima/data/instance.json' 2>/dev/null | head -n1
}
field() {
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$1" "$2"
}

echo "== Second prefix at $PREFIX2"
WINEPREFIX="$PREFIX2" "$WINE" wineboot -i >/dev/null 2>&1 || true
WINEPREFIX="$PREFIX2" "$WINE" "$SETUP" /S
WINEPREFIX="$PREFIX2" "$WINE" "$CLI" list-games --json >/dev/null 2>&1 &

inst1="$(instance_file "$WINEPREFIX")"
inst2=""
for _ in $(seq 1 60); do
    inst2="$(instance_file "$PREFIX2")"
    [[ -n "$inst2" ]] && break
    sleep 1
done
[[ -n "$inst1" ]] || fail "first prefix has no instance.json"
[[ -n "$inst2" ]] || fail "second prefix's server never published instance.json"

port1="$(field "$inst1" control_port)"; port2="$(field "$inst2" control_port)"
realm1="$(field "$inst1" realm)"; realm2="$(field "$inst2" realm)"
echo "   prefix 1: realm $realm1 port $port1"
echo "   prefix 2: realm $realm2 port $port2"
[[ "$port1" != "$port2" && "$realm1" != "$realm2" ]] || fail "the two prefixes share a server identity"
ok "each prefix runs its own server"

for pair in "$WINEPREFIX:$realm1" "$PREFIX2:$realm2"; do
    prefix="${pair%%:*}"; realm="${pair##*:}"
    status="$(WINEPREFIX="$prefix" "$WINE" "$CLI" server-status --json 2>/dev/null | tr -d '\r' | grep '^{' || true)"
    echo "$status" | grep -q "\"realm\":\"$realm\"" \
        || fail "server-status in $prefix did not reach its own server: $status"
done
ok "server-status in each prefix reaches its own server"

reply="$(python3 - "$port1" "$(field "$inst2" token)" <<'PY'
import json, socket, sys
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=5)
s.sendall((json.dumps({"id": 1, "cmd": "hello", "token": sys.argv[2], "proto": 2}) + "\n").encode())
print(s.makefile().readline().strip())
PY
)"
echo "   prefix 2's token at prefix 1's server: $reply"
echo "$reply" | grep -q '"kind":"unauthorized"' || fail "a server accepted another prefix's token"
ok "a server refuses another prefix's token"

echo "All Wine smoke checks passed."
