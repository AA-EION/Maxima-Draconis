#!/bin/bash
# Smoke-test MaximaSetup.exe in a throwaway Wine prefix — the in-bottle mode
# a consumer launcher drives through CrossOver. No EA login is possible on CI, so this
# checks what can fail without one: the installed layout, the protocol
# handlers, that the Run-key autostart is skipped under Wine, and that the
# thin-client CLI really spawns maxima-server.exe (and that the server, waiting
# on a login that never comes, doesn't spin a core on its closed stdin).
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

cleanup() {
    "$WINESERVER" -k >/dev/null 2>&1 || true
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
echo "All Wine smoke checks passed."
