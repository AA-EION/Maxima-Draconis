# Linux packaging

Three ways to ship the Linux build of Maxima. All of them package the same
binaries: `maxima-cli`, `maxima-server`, `maxima-bootstrap` and `maxima-tui`.

`maxima-ui` (the egui frontend) is not packaged: it pulls `rustix 0.37` through
`accesskit_unix -> zbus 3 -> async-io 1.13`, which does not compile on current
nightly. `maxima-service` is Windows-only.

| Path | What it is |
|---|---|
| `Containerfile`, `build-container.sh` | Reproducible static (musl) release build in podman/docker |
| `appimage/build.sh` | Builds `Maxima-<version>-<arch>.AppImage` from those binaries |
| `flatpak/com.ArmchairDevelopers.Maxima.yml` | Flatpak manifest, built from source |
| `maxima-launcher.sh` | Entry point shared by AppImage (`AppRun`) and Flatpak (`/app/bin/maxima`) |
| `com.ArmchairDevelopers.Maxima.desktop` | Declares the `link2ea`, `origin2` and `qrc` URL scheme handlers |
| `com.ArmchairDevelopers.Maxima.metainfo.xml` | AppStream metadata (AppImage and Flatpak) |
| `validate.sh` | Static checks of everything above (what CI runs) |

The app id is `com.ArmchairDevelopers.Maxima`, matching the
`ProjectDirs("com", "ArmchairDevelopers", "Maxima")` data directories.

## 1. Static binaries via container

```sh
packaging/linux/build-container.sh            # -> dist/linux/
packaging/linux/build-container.sh out/ --build-arg RUST_TOOLCHAIN=nightly-2026-10-07
packaging/linux/build-container.sh out/ --build-arg BUILD_TUI=0
```

Uses podman if present, otherwise docker (no buildx needed). With buildx or
podman you can also export directly:

```sh
podman build -f packaging/linux/Containerfile --target artifacts \
    --output type=local,dest=dist/linux .
```

The binaries are linked statically against musl (`-C target-feature=+crt-static`),
so they run on any distribution. `rust-toolchain.toml` only says `nightly`; pass
a dated toolchain (`RUST_TOOLCHAIN`) for a bit-for-bit reproducible compiler.
The image installs `protobuf-compiler` (prost), `musl-tools` (ring),
`libdbus-1-dev` and `pkg-config` (only for the optional `linux-tray` feature of
`maxima-server`).

Without a container, the equivalent is:

```sh
rustup target add x86_64-unknown-linux-musl
RUSTFLAGS="-C target-feature=+crt-static" cargo build --release \
    --target x86_64-unknown-linux-musl \
    -p maxima-cli -p maxima-server -p maxima-bootstrap -p maxima-tui
```

## 2. AppImage

```sh
packaging/linux/appimage/build.sh [BIN_DIR] [OUT_DIR]
# defaults: target/<arch>-unknown-linux-musl/release  ->  dist/
```

The script downloads `appimagetool` 1.9.0 and verifies its pinned SHA-256
(x86_64 and aarch64), assembles the AppDir, and runs it with
`APPIMAGE_EXTRACT_AND_RUN=1`, so FUSE is not needed to build. (`appimagetool`
itself fetches the AppImage runtime, which upstream only publishes under a
rolling tag, so the runtime is not checksum-pinned.) Set `APPIMAGETOOL=/path`
to use a local copy. The build needs `desktop-file-utils` (and `appstream` for
the metadata check); `ALLOW_DYNAMIC=1` accepts non-static input binaries, which
this recipe does not bundle libraries for.

Running it:

```sh
./Maxima-0.16.0-x86_64.AppImage                   # interactive maxima-cli menu
./Maxima-0.16.0-x86_64.AppImage launch <slug>     # any maxima-cli arguments
./Maxima-0.16.0-x86_64.AppImage server            # maxima-server (also: tui, bootstrap, cli)
./Maxima-0.16.0-x86_64.AppImage link2ea://...     # URLs go to maxima-bootstrap
```

Dispatch (`maxima-launcher.sh`): invoked as `maxima-cli`/`maxima-server`/
`maxima-bootstrap`/`maxima-tui` (symlink name, via `$ARGV0`) runs that binary; a
`link2ea:`/`origin2:`/`qrc:` URL as first argument runs `maxima-bootstrap`; a
first argument of `cli|server|bootstrap|tui` (or the full `maxima-*` name)
selects the binary; anything else goes to `maxima-cli`.

### URL scheme handlers

The AppImage's `.desktop` file declares
`MimeType=x-scheme-handler/link2ea;x-scheme-handler/origin2;x-scheme-handler/qrc;`
with `Exec=... %u`, the same scheme handlers `maxima-lib` registers itself
(`maxima-<scheme>.desktop` with `MimeType=x-scheme-handler/<scheme>` and
`Exec=<maxima-bootstrap> %u`, see `register_custom_protocol` in
`maxima-lib/src/util/registry.rs`). Because an AppImage path is not stable, the
launcher sets `MAXIMA_PACKAGED=1` (skip that self-registration, the existing
switch for packaged builds). The startup handler check accepts either
`maxima-qrc.desktop` or the package's `com.ArmchairDevelopers.Maxima.desktop`.

The handlers become active when the AppImage is integrated by AppImageLauncher /
appimaged, or by hand:

```sh
./Maxima-0.16.0-x86_64.AppImage --install-handlers     # writes ~/.local/share/applications/com.ArmchairDevelopers.Maxima.desktop, runs xdg-mime default
./Maxima-0.16.0-x86_64.AppImage --uninstall-handlers
```

Without any registration, login still works through the paste-the-redirect-URL
fallback.

## 3. Flatpak

Runtime `org.freedesktop.Platform//24.08`, SDK extension
`org.freedesktop.Sdk.Extension.rust-nightly` (the workspace needs nightly;
`rust-stable` will not do). `protoc` is built in as a pinned prebuilt module,
since the SDK does not ship it.

```sh
flatpak install flathub org.freedesktop.Platform//24.08 org.freedesktop.Sdk//24.08 \
    org.freedesktop.Sdk.Extension.rust-nightly//24.08
pip install aiohttp tomlkit            # and fetch flatpak-cargo-generator.py
FLATPAK_CARGO_GENERATOR=/path/to/flatpak-cargo-generator.py \
    packaging/linux/flatpak/generate-sources.sh      # writes cargo-sources.json
cd packaging/linux/flatpak
flatpak-builder --user --install --force-clean build-dir com.ArmchairDevelopers.Maxima.yml
flatpak run com.ArmchairDevelopers.Maxima
```

`cargo-sources.json` (every crate plus the two git dependencies) is generated
from `Cargo.lock` and is git-ignored. Flatpak builds have no network, hence the
vendored sources and `cargo --offline`. For publishing, replace the local
`type: dir` source with a `type: git` one.

Permissions (`finish-args`): `--share=network` (EA, CDN; the loopback services
are covered by it), `--filesystem=~/Games:create` for downloads, and the
`MAXIMA_PACKAGED=1` environment. Settings, tokens and the
cache live in the sandbox's own `~/.var/app/com.ArmchairDevelopers.Maxima`
directories, which need no permission. URL scheme handling comes from the
exported `.desktop` file, and the login page is opened through the OpenURI
portal via `xdg-open`.

Limitation: Wine/Proton is not inside the sandbox. The Flatpak covers login,
library, install/update, the server and the protocol handler; running a game
under Proton from within the sandbox needs extra escapes that are intentionally
not granted.

## Checking the files without building

```sh
packaging/linux/validate.sh          # REQUIRE_TOOLS=1 to fail when tools are missing
```

It runs `sh -n`/`bash -n` on the scripts, `desktop-file-validate`,
`appstreamcli validate --no-net` (tolerating exactly one known error, below),
checks that the metainfo has a `<release>` for the workspace version, and
parses the Flatpak manifest. `appstreamcli` flags `cid-domain-not-lowercase` for
`com.ArmchairDevelopers.Maxima`; the mixed-case id is required to match the data
directories, so that one finding is ignored.

When bumping the workspace version, add a matching `<release>` to the metainfo
or `validate.sh` (and the CI job) fails.

## CI

The `linux-packages` job in `.github/workflows/build-ci.yml` runs
`validate.sh`, builds the static binaries, builds the AppImage, smoke-tests it
(`--help`, `cli --help`, `server-status --json`, contents) and uploads it as the
`maxima-appimage-x86_64` artifact. It does not run `flatpak-builder`.
