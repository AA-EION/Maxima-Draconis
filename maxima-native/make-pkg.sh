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
# Runs as root. Symlink the binaries onto PATH, then — as the console user, the
# owner of the per-user launchd + LaunchServices databases — register the
# service and the URL protocol handlers. Both steps are login-free (service
# install with on-demand only writes config + syncs binaries; lsregister just
# claims the schemes), so installing never opens a browser.
#
# `set -u` (not -e): the symlink loop's `[ -f ] &&` idioms return non-zero when
# a binary is absent, and we don't want that to abort the whole script.
set -u
RES="/Applications/Maxima.app/Contents/Resources"
CLI="${RES}/maxima-cli"
mkdir -p /usr/local/bin
for b in maxima-cli maxima-server maxima-bootstrap maxima-tui; do
    if [ -f "${RES}/${b}" ]; then
        ln -sf "${RES}/${b}" "/usr/local/bin/${b}"
    fi
done

LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
BOOTSTRAP_APP="${RES}/bundle/osx/MaximaBootstrap.app"

CONSOLE_USER="$(stat -f%Su /dev/console)"
if [ -n "$CONSOLE_USER" ] && [ "$CONSOLE_USER" != "root" ] && [ -x "$CLI" ]; then
    CONSOLE_UID="$(id -u "$CONSOLE_USER")"
    # Call the CLI by its real bundle path (the /usr/local/bin symlink may not
    # be on the sandboxed script's PATH).
    launchctl asuser "$CONSOLE_UID" sudo -u "$CONSOLE_USER" \
        "$CLI" service install --boot on-demand || true
    # Register qrc:// / link2ea:// / origin2:// (no login needed).
    if [ -d "$BOOTSTRAP_APP" ]; then
        launchctl asuser "$CONSOLE_UID" sudo -u "$CONSOLE_USER" \
            "$LSREGISTER" -f "$BOOTSTRAP_APP" || true
    fi
fi
exit 0
POST

# Disable macOS "bundle relocation" — otherwise PackageKit installs on top of
# an already-registered copy of Maxima.app (e.g. this build dir) instead of
# /Applications. Force the fixed install path.
cat > "${STAGE}/component.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<array>
    <dict>
        <key>BundleHasStrictIdentifier</key>
        <true/>
        <key>BundleIsRelocatable</key>
        <false/>
        <key>BundleIsVersionChecked</key>
        <false/>
        <key>BundleOverwriteAction</key>
        <string>upgrade</string>
        <key>RootRelativeBundlePath</key>
        <string>Applications/Maxima.app</string>
    </dict>
</array>
</plist>
PLIST
chmod +x "${SCRIPTS}/postinstall"

echo "[1/2] pkgbuild…"
pkgbuild --root "$ROOT" \
    --identifier "$IDENT" \
    --version "$VERSION" \
    --scripts "$SCRIPTS" \
    --component-plist "${STAGE}/component.plist" \
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
