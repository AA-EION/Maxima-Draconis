#!/bin/bash
# Build Maxima-Installer.pkg — installs Maxima.app into /Applications and, in a
# postinstall, symlinks the CLI/TUI onto PATH and registers the background
# service for the console user. A pkg-placed app is NOT translocated, so its
# bundled binaries sit at a stable path.
#
# Uninstall (leaves no trace):
#   maxima-cli service uninstall [--purge]   # or run the generated uninstall.sh
#
# Usage: bash maxima-native/build.sh && bash maxima-native/make-pkg.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BUILD_DIR="${SCRIPT_DIR}/build"
APP="${BUILD_DIR}/Maxima.app"
IDENT="com.armchairdevelopers.maxima.native"
VERSION="1.0"
OUT="${BUILD_DIR}/Maxima-Installer.pkg"

if [[ ! -d "$APP" ]]; then
    echo "error: $APP not found — build it first: bash maxima-native/build.sh" >&2
    exit 1
fi

STAGE="$(mktemp -d)"
ROOT="${STAGE}/root"
SCRIPTS="${STAGE}/scripts"
mkdir -p "${ROOT}/Applications" "${SCRIPTS}"
cp -R "$APP" "${ROOT}/Applications/"

cat > "${SCRIPTS}/postinstall" <<'POST'
#!/bin/bash
# Runs as root. Symlink the binaries onto PATH and register the service as the
# console user (the launchd agent is per-user, not root).
set -e
RES="/Applications/Maxima.app/Contents/Resources"
mkdir -p /usr/local/bin
for b in maxima-cli maxima-server maxima-bootstrap maxima-tui; do
    [ -f "${RES}/${b}" ] && ln -sf "${RES}/${b}" "/usr/local/bin/${b}"
done

CONSOLE_USER="$(stat -f%Su /dev/console)"
if [ -n "$CONSOLE_USER" ] && [ "$CONSOLE_USER" != "root" ]; then
    CONSOLE_UID="$(id -u "$CONSOLE_USER")"
    launchctl asuser "$CONSOLE_UID" sudo -u "$CONSOLE_USER" \
        /usr/local/bin/maxima-cli service install --boot on-demand || true
fi
exit 0
POST
chmod +x "${SCRIPTS}/postinstall"

echo "[1/2] pkgbuild…"
pkgbuild --root "$ROOT" \
    --identifier "$IDENT" \
    --version "$VERSION" \
    --scripts "$SCRIPTS" \
    --install-location / \
    "${BUILD_DIR}/Maxima-component.pkg"

echo "[2/2] productbuild…"
productbuild --package "${BUILD_DIR}/Maxima-component.pkg" "$OUT"
rm -f "${BUILD_DIR}/Maxima-component.pkg"
rm -rf "$STAGE"

# Generate an uninstaller for users who removed the CLI first.
cat > "${BUILD_DIR}/uninstall.sh" <<'UNINST'
#!/bin/bash
# Remove Maxima completely (no trace).
[ -x /usr/local/bin/maxima-cli ] && /usr/local/bin/maxima-cli service uninstall "$@" || true
rm -f /usr/local/bin/maxima-cli /usr/local/bin/maxima-server \
      /usr/local/bin/maxima-bootstrap /usr/local/bin/maxima-tui
rm -rf /Applications/Maxima.app
echo "Maxima removed."
UNINST
chmod +x "${BUILD_DIR}/uninstall.sh"

echo ""
echo "Built ${OUT}"
echo "Uninstaller: ${BUILD_DIR}/uninstall.sh"
echo "Note: pkg is unsigned (ad-hoc) — install with: sudo installer -pkg '${OUT}' -target /"
echo "      or right-click → Open in Finder to bypass Gatekeeper."
