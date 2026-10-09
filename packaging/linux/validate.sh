#!/bin/bash
# Static checks for the Linux packaging files (no building, no network).
#
#   packaging/linux/validate.sh
#
# Needs: python3 + PyYAML, and optionally desktop-file-validate and appstreamcli
# (apt: desktop-file-utils appstream). Missing optional tools are reported, not
# fatal, unless REQUIRE_TOOLS=1.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
app_id="com.ArmchairDevelopers.Maxima"
fail=0

need() {
    if command -v "$1" >/dev/null 2>&1; then
        return 0
    fi
    echo "SKIP: $1 not installed"
    if [ "${REQUIRE_TOOLS:-0}" = 1 ]; then
        fail=1
    fi
    return 1
}

echo "== shell syntax"
for f in "$here/maxima-launcher.sh"; do
    sh -n "$f"
done
for f in "$here/build-container.sh" "$here/validate.sh" \
         "$here/appimage/build.sh" "$here/flatpak/generate-sources.sh"; do
    bash -n "$f"
done

echo "== desktop file"
if need desktop-file-validate; then
    desktop-file-validate "$here/$app_id.desktop"
fi
for scheme in link2ea origin2 qrc; do
    grep -q "x-scheme-handler/$scheme;" "$here/$app_id.desktop" \
        || { echo "desktop file lacks x-scheme-handler/$scheme"; fail=1; }
done

echo "== metainfo"
# The app id is mandated to be mixed-case (it matches the ProjectDirs
# qualifier/organisation), which appstreamcli reports as one error; anything
# else is a real failure.
if need appstreamcli; then
    out="$(appstreamcli validate --no-net "$here/$app_id.metainfo.xml" 2>&1 || true)"
    echo "$out"
    if echo "$out" | grep '^[EW]:' | grep -v 'cid-domain-not-lowercase' | grep -q .; then
        fail=1
    fi
else
    xmllint --noout "$here/$app_id.metainfo.xml" 2>/dev/null || python3 -I - "$here/$app_id.metainfo.xml" <<'PY'
import sys, xml.dom.minidom
xml.dom.minidom.parse(sys.argv[1])
PY
fi

echo "== metainfo release matches workspace version"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -n1)"
grep -q "<release version=\"$version\"" "$here/$app_id.metainfo.xml" \
    || { echo "metainfo has no <release version=\"$version\"> (workspace version)"; fail=1; }

echo "== flatpak manifest"
python3 -I - "$here/flatpak/$app_id.yml" "$app_id" <<'PY'
import sys, yaml
m = yaml.safe_load(open(sys.argv[1]))
assert m["app-id"] == sys.argv[2], "app-id mismatch"
for key in ("runtime", "runtime-version", "sdk", "command", "finish-args", "modules"):
    assert key in m, f"missing {key}"
assert "org.freedesktop.Sdk.Extension.rust-nightly" in m["sdk-extensions"]
for mod in m["modules"]:
    assert "name" in mod and "sources" in mod, mod
print("ok:", [mod["name"] for mod in m["modules"]])
PY

exit "$fail"
