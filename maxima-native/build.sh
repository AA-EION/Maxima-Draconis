#!/bin/bash
# Maxima native macOS UI — build script.
#
# Assembles Maxima.app (SwiftUI, Liquid Glass, macOS 26+) from
# Sources/*.swift, and bundles the native maxima-cli / maxima-bootstrap /
# MaximaBootstrap.app into Contents/Resources when they've been built —
# making the .app self-contained. Without bundled binaries the app falls
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
</dict>
</plist>
PLIST

if $BUNDLE_BINARIES; then
    echo "[3/4] Bundling maxima binaries (when built)..."
    for bin in maxima-cli maxima-server maxima-bootstrap; do
        if [[ -f "${RELEASE_DIR}/${bin}" ]]; then
            cp "${RELEASE_DIR}/${bin}" "${APP}/Contents/Resources/${bin}"
            echo "  + ${bin}"
        else
            echo "  - ${bin} not built (cargo build --release -p ${bin}); app will use the dev-tree fallback"
        fi
    done
    # register-protocols expects the bundle next to the CLI:
    # <cli dir>/bundle/osx/MaximaBootstrap.app
    if [[ -d "${RELEASE_DIR}/bundle/osx/MaximaBootstrap.app" ]]; then
        mkdir -p "${APP}/Contents/Resources/bundle/osx"
        cp -R "${RELEASE_DIR}/bundle/osx/MaximaBootstrap.app" \
              "${APP}/Contents/Resources/bundle/osx/"
        echo "  + MaximaBootstrap.app"
    else
        echo "  - MaximaBootstrap.app not built (bash maxima-bootstrap/build-app.sh)"
    fi
else
    echo "[3/4] Skipping binary bundling (--skip-bundle-binaries)"
fi

echo "[4/4] Signing..."
codesign --force --deep --sign - "${APP}"

echo ""
echo "Built ${APP}"
echo "Run:  open '${APP}'"
