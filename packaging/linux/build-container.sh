#!/bin/bash
# Build the Linux release binaries inside the Containerfile and export them.
#
#   packaging/linux/build-container.sh [OUT_DIR] [extra build args...]
#
# OUT_DIR defaults to dist/linux. Uses podman if installed, otherwise docker.
# Extra args go to `build`, e.g. --build-arg RUST_TOOLCHAIN=nightly-2026-10-07
# or --build-arg BUILD_TUI=0 (skip maxima-tui). Works with plain `docker build` (no buildx
# needed): the binaries are copied out of a throwaway container.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
out="${1:-$repo/dist/linux}"
shift || true

if command -v podman >/dev/null 2>&1; then
    engine=podman
elif command -v docker >/dev/null 2>&1; then
    engine=docker
else
    echo "neither podman nor docker found" >&2
    exit 1
fi

tag="maxima-linux-build:local"
"$engine" build -f "$here/Containerfile" --target artifacts -t "$tag" "$@" "$repo"

mkdir -p "$out"
cid="$("$engine" create "$tag" /nonexistent)"
trap '"$engine" rm -f "$cid" >/dev/null 2>&1 || true' EXIT
"$engine" cp "$cid:/." "$out/"
ls -l "$out"
