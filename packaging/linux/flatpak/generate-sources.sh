#!/bin/bash
# Generate cargo-sources.json (vendored crate list for the offline Flatpak build)
# from the workspace Cargo.lock.
#
#   packaging/linux/flatpak/generate-sources.sh
#
# Needs flatpak-cargo-generator from https://github.com/flatpak/flatpak-builder-tools
# (cargo/flatpak-cargo-generator.py; deps: aiohttp, tomlkit). Either put it on
# PATH as `flatpak-cargo-generator`, or point FLATPAK_CARGO_GENERATOR at the .py.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"

if [ -n "${FLATPAK_CARGO_GENERATOR:-}" ]; then
    gen=(python3 "$FLATPAK_CARGO_GENERATOR")
elif command -v flatpak-cargo-generator >/dev/null 2>&1; then
    gen=(flatpak-cargo-generator)
else
    echo "flatpak-cargo-generator not found; see the header of this script" >&2
    exit 1
fi

"${gen[@]}" "$repo/Cargo.lock" -o "$here/cargo-sources.json"
echo "wrote $here/cargo-sources.json"
