#!/bin/bash
# Maxima native macOS UI — build script.
#
# Assembles Maxima.app (SwiftUI, Liquid Glass, macOS 26+) from
# Sources/*.swift, and bundles the native maxima-server / maxima-cli /
# maxima-bootstrap into Contents/Resources when they've been built —
# making the .app self-contained. The app itself handles qrc:// / link2ea:// /
# origin2://. Without bundled binaries the app falls
# back to the dev-tree target/release lookup (see MaximaCLI.locate()).
#
# Usage: bash maxima-native/build.sh [--skip-bundle-binaries]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
BUILD_DIR="${SCRIPT_DIR}/build"
APP="${BUILD_DIR}/Maxima.app"
RELEASE_DIR="${PROJECT_ROOT}/target/release"
BUNDLE_BINARIES=true

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-bundle-binaries) BUNDLE_BINARIES=false; shift ;;
        *) echo "Unknown argument: $1"; exit 1 ;;
    esac
done

if ! command -v swiftc &>/dev/null; then
    echo "ERROR: swiftc not found. Install Xcode Command Line Tools." >&2
    exit 1
fi

echo "[1/4] Compiling Swift sources (macOS 26+, arm64)..."
rm -rf "${APP}"
mkdir -p "${APP}/Contents/MacOS" "${APP}/Contents/Resources"

SDK="$(xcrun --show-sdk-path)"
swiftc -O -sdk "${SDK}" \
    -target arm64-apple-macos26.0 \
    -swift-version 5 \
    -parse-as-library \
    -framework SwiftUI -framework AppKit \
    "${SCRIPT_DIR}"/Sources/*.swift \
    -o "${APP}/Contents/MacOS/Maxima"

echo "[1b/4] Generating AppIcon.icns from the Maxima logo..."
LOGO="${PROJECT_ROOT}/maxima-resources/assets/logo.png"
if command -v iconutil &>/dev/null && command -v sips &>/dev/null && [[ -f "$LOGO" ]]; then
    ICONSET="${BUILD_DIR}/AppIcon.iconset"
    rm -rf "$ICONSET"; mkdir -p "$ICONSET"
    for size in 16 32 128 256 512; do
        sips -z "$size" "$size" "$LOGO" --out "${ICONSET}/icon_${size}x${size}.png" >/dev/null
        dbl=$((size * 2))
        sips -z "$dbl" "$dbl" "$LOGO" --out "${ICONSET}/icon_${size}x${size}@2x.png" >/dev/null
    done
    iconutil -c icns "$ICONSET" -o "${APP}/Contents/Resources/AppIcon.icns"
    rm -rf "$ICONSET"
    echo "  + AppIcon.icns"
else
    echo "  - skipped (need iconutil + sips + $LOGO)"
fi

echo "[2/4] Writing Info.plist..."
cat > "${APP}/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Maxima</string>
    <key>CFBundleDisplayName</key>
    <string>Maxima</string>
    <key>CFBundleIdentifier</key>
    <string>com.armchairdevelopers.maxima.native</string>
    <key>CFBundleExecutable</key>
    <string>Maxima</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleVersion</key>
    <string>1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0</string>
    <key>LSMinimumSystemVersion</key>
    <string>26.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.games</string>
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key>
            <string>com.armchairdevelopers.maxima.native</string>
            <key>CFBundleURLSchemes</key>
            <array>
                <string>qrc</string>
                <string>link2ea</string>
                <string>origin2</string>
            </array>
        </dict>
    </array>
</dict>
</plist>
PLIST

if $BUNDLE_BINARIES; then
    echo "[3/4] Bundling maxima binaries (when built)..."
    for bin in maxima-cli maxima-server maxima-bootstrap maxima-tui; do
        if [[ -f "${RELEASE_DIR}/${bin}" ]]; then
            cp "${RELEASE_DIR}/${bin}" "${APP}/Contents/Resources/${bin}"
            echo "  + ${bin}"
        else
            echo "  - ${bin} not built (cargo build --release -p ${bin}); app will use the dev-tree fallback"
        fi
    done
else
    echo "[3/4] Skipping binary bundling (--skip-bundle-binaries)"
fi

echo "[4/4] Signing..."
codesign --force --deep --sign - "${APP}"

echo ""
echo "Built ${APP}"
echo "Run:  open '${APP}'"
