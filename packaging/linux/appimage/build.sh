#!/bin/bash
# Build Maxima-<version>-<arch>.AppImage from already-built Linux release binaries.
#
#   packaging/linux/appimage/build.sh [BIN_DIR] [OUT_DIR]
#
# BIN_DIR  directory holding maxima-cli, maxima-server, maxima-bootstrap
#          (and optionally maxima-tui).
#          Default: ${CARGO_TARGET_DIR:-target}/<arch>-unknown-linux-musl/release
# OUT_DIR  where the AppImage is written. Default: dist/
#
# Environment:
#   APPIMAGETOOL   path to an existing appimagetool; skips the download
#   APPIMAGE_WORK  scratch dir (default: a fresh mktemp dir, removed on exit)
#   ALLOW_DYNAMIC  set to 1 to accept dynamically linked binaries (they would
#                  need their shared libraries bundled, which this script does
#                  not do - the supported input is the static musl build)
set -euo pipefail

APPIMAGETOOL_VERSION="1.9.0"
APPIMAGETOOL_SHA256_x86_64="46fdd785094c7f6e545b61afcfb0f3d98d8eab243f644b4b17698c01d06083d1"
APPIMAGETOOL_SHA256_aarch64="04f45ea45b5aa07bb2b071aed9dbf7a5185d3953b11b47358c1311f11ea94a96"

APP_ID="com.ArmchairDevelopers.Maxima"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
pkg="$(cd "$here/.." && pwd)"
repo="$(cd "$pkg/../.." && pwd)"

arch="$(uname -m)"
case "$arch" in
    x86_64|aarch64) ;;
    *) echo "unsupported architecture: $arch (appimagetool ships x86_64 and aarch64)" >&2; exit 1 ;;
esac

bin_dir="${1:-${CARGO_TARGET_DIR:-$repo/target}/${arch}-unknown-linux-musl/release}"
out_dir="${2:-$repo/dist}"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -n1)"
if [ -z "$version" ]; then
    echo "could not read the workspace version from Cargo.toml" >&2
    exit 1
fi

for b in maxima-cli maxima-server maxima-bootstrap; do
    if [ ! -x "$bin_dir/$b" ]; then
        echo "missing $bin_dir/$b - build it first (see packaging/linux/README.md)" >&2
        exit 1
    fi
done

if [ "${ALLOW_DYNAMIC:-0}" != 1 ] && command -v ldd >/dev/null 2>&1; then
    for b in "$bin_dir"/maxima-cli "$bin_dir"/maxima-server "$bin_dir"/maxima-bootstrap; do
        if ldd "$b" >/dev/null 2>&1 && ! ldd "$b" 2>&1 | grep -q 'not a dynamic executable\|statically linked'; then
            echo "$b is dynamically linked; build with the musl target + RUSTFLAGS='-C target-feature=+crt-static'" >&2
            echo "(or set ALLOW_DYNAMIC=1 if you know the target hosts provide every library)" >&2
            exit 1
        fi
    done
fi

work="${APPIMAGE_WORK:-$(mktemp -d)}"
if [ -z "${APPIMAGE_WORK:-}" ]; then
    trap 'rm -rf "$work"' EXIT
fi
mkdir -p "$work" "$out_dir"

tool="${APPIMAGETOOL:-}"
if [ -z "$tool" ]; then
    sha_var="APPIMAGETOOL_SHA256_${arch}"
    expected="${!sha_var}"
    tool="$work/appimagetool-$arch.AppImage"
    url="https://github.com/AppImage/appimagetool/releases/download/${APPIMAGETOOL_VERSION}/appimagetool-${arch}.AppImage"
    echo "==> downloading appimagetool ${APPIMAGETOOL_VERSION} (${arch})"
    curl -fsSL --retry 5 --retry-connrefused -o "$tool" "$url"
    echo "$expected  $tool" | sha256sum -c -
    chmod +x "$tool"
fi

appdir="$work/Maxima.AppDir"
rm -rf "$appdir"
mkdir -p "$appdir/usr/bin" \
         "$appdir/usr/share/applications" \
         "$appdir/usr/share/metainfo" \
         "$appdir/usr/share/icons/hicolor/32x32/apps"

echo "==> assembling AppDir from $bin_dir"
for b in maxima-cli maxima-server maxima-bootstrap maxima-tui; do
    if [ -x "$bin_dir/$b" ]; then
        install -m 0755 "$bin_dir/$b" "$appdir/usr/bin/$b"
    fi
done
install -m 0755 "$pkg/maxima-launcher.sh" "$appdir/AppRun"

install -m 0644 "$pkg/$APP_ID.desktop" "$appdir/$APP_ID.desktop"
install -m 0644 "$pkg/$APP_ID.desktop" "$appdir/usr/share/applications/$APP_ID.desktop"
install -m 0644 "$pkg/$APP_ID.metainfo.xml" "$appdir/usr/share/metainfo/$APP_ID.metainfo.xml"

icon="$repo/maxima-resources/assets/logo.png"
install -m 0644 "$icon" "$appdir/$APP_ID.png"
install -m 0644 "$icon" "$appdir/.DirIcon"
install -m 0644 "$icon" "$appdir/usr/share/icons/hicolor/32x32/apps/$APP_ID.png"

out="$out_dir/Maxima-${version}-${arch}.AppImage"
echo "==> running appimagetool"
# APPIMAGE_EXTRACT_AND_RUN lets appimagetool run where FUSE is unavailable
# (containers, most CI runners).
ARCH="$arch" APPIMAGE_EXTRACT_AND_RUN=1 "$tool" "$appdir" "$out"
chmod +x "$out"

echo "==> built $out"
