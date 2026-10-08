#!/bin/bash
# Assembles MaximaBootstrap.app from the compiled native macOS binary.
#
# Output: <profile-dir>/bundle/osx/MaximaBootstrap.app — the layout
# maxima-lib's bootstrap_path() / set_up_registry() expect. The bundle is
# what LaunchServices registers for qrc:// / link2ea:// / origin2://; the
# bare binary next to maxima-cli keeps handling the direct launch spawns.
#
# Usage: bash maxima-bootstrap/build-app.sh [profile-dir]
#   profile-dir defaults to target/release (relative to the repo root).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE_DIR="${1:-$ROOT/target/release}"
BIN="$PROFILE_DIR/maxima-bootstrap"

if [[ ! -f "$BIN" ]]; then
    echo "error: $BIN not found — build it first:" >&2
    echo "  cargo build --release -p maxima-bootstrap" >&2
    exit 1
fi

APP="$PROFILE_DIR/bundle/osx/MaximaBootstrap.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp "$BIN" "$APP/Contents/MacOS/maxima-bootstrap"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>MaximaBootstrap</string>
    <key>CFBundleDisplayName</key>
    <string>Maxima Bootstrap</string>
    <key>CFBundleIdentifier</key>
    <string>com.armchairdevelopers.maxima.bootstrap</string>
    <key>CFBundleExecutable</key>
    <string>maxima-bootstrap</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleVersion</key>
    <string>1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key>
            <string>Maxima Protocol</string>
            <key>CFBundleURLSchemes</key>
            <array>
                <string>qrc</string>
            </array>
        </dict>
        <dict>
            <key>CFBundleURLName</key>
            <string>Maxima Launcher</string>
            <key>CFBundleURLSchemes</key>
            <array>
                <string>link2ea</string>
                <string>origin2</string>
            </array>
        </dict>
    </array>
</dict>
</plist>
PLIST

# Bundle-seal the signature. LaunchServices SILENTLY ignores URL scheme
# claims from bundles whose Info.plist isn't sealed into the signature
# (linker-signed-only binaries) — same gotcha MaximaHelper.app hit; see
# CLAUDE.md "Signing gotcha".
codesign --force --deep --sign - "$APP"

echo "Built $APP"
