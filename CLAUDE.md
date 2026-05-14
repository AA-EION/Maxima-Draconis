# Maxima-Draconis — engineering reference for Claude agents

This is the **Maxima-Draconis fork** — the EA authentication and launch backend used by [Draconis](https://github.com/AA-EION/Draconis), a native macOS launcher for Titanfall 2 on CrossOver / Wine. This file is the living engineering reference for anyone picking up the repo cold. It covers architecture, known gotchas, diagnostics, and a running changelog.

---

## What Maxima is

Open-source replacement for the EA Desktop Launcher. **Not** a macOS-native app — `maxima-cli` / `maxima-bootstrap` / `maxima-service` are Windows binaries that run **inside the CrossOver bottle** alongside Titanfall 2. The only piece that runs on the macOS host is `MaximaHelper.app`, a tiny Swift background agent that bridges EA's `qrc://` OAuth redirect from the user's browser into the bottle.

The Draconis fork is tested *only* for Titanfall 2 on macOS via CrossOver. Other configurations may work but aren't supported here.

### Multi-OS compatibility principle

Even though the active maintenance target is macOS/CrossOver, **the code must remain compatible with the other OSes upstream supports** — Linux (native + musl) and native Windows. Concretely:

- All `#[cfg(unix)]`, `#[cfg(target_os = "linux")]`, `#[cfg(target_os = "macos")]`, `#[cfg(windows)]`, `#[cfg(not(windows))]` gates that exist in upstream must be preserved when editing the affected file.
- Don't introduce hard `panic!()` or `unimplemented!()` on a code path that other OSes hit at runtime.
- Don't add `#[cfg]`-gated dependencies that would skip building on other targets without a clear reason; if you need to, scope the gate as narrowly as possible.
- `maxima-ui` and `maxima-tui` are **upstream graphical/TUI frontends** that this fork does not actively maintain. They are excluded from this fork's CI (see "CI" section) because `maxima-ui` transitively pulls a `rustix 0.37` version that doesn't build on modern nightly. They remain in `Cargo.toml`'s `[workspace.members]` so a downstream consumer who wants them can build them locally. **Do not delete them.**
- The Linux CI job builds `maxima-cli` + `maxima-bootstrap` to make sure the cross-platform code paths actually compile on a non-macOS unix. The Windows CI job builds the three Draconis-relevant crates **and** the NSIS installer. If you touch `#[cfg(unix)]` or `#[cfg(windows)]` blocks, make sure those jobs still pass.

In short: macOS/CrossOver is what we **test**, but the codebase is **portable** to the same targets upstream supports.

---

## Component layout

```
macOS host
├── Draconis.app           — SwiftUI launcher (in AA-EION/Draconis)
│   └── Contents/Resources/
│       └── MaximaHelper.app — qrc:// → http://127.0.0.1:31033 bridge
│                              (built from MaximaHelper/ in this repo)
│
└── CrossOver bottle (Wine prefix)
    └── Program Files (x86)/Maxima/
        ├── maxima-cli.exe         — auth + launch CLI
        ├── maxima-bootstrap.exe   — link2ea:// / origin2:// / qrc:// handler
        ├── maxima-service.exe     — background service (DLL injection, registry setup)
        └── Uninstall.exe          — NSIS uninstaller from MaximaSetup.exe
```

Build outputs:

- `installer/MaximaSetup.exe` — NSIS bundle that installs everything in the bottle and registers the protocol handlers in Wine's registry. Cross-compiled on macOS via `mingw-w64` + `nsis`.
- `MaximaHelper/build/MaximaHelper.app` — built on macOS with Xcode CLT.
- `MaximaHelper.zip` — release asset Draconis downloads at build time.

---

## Workspace inventory

```
maxima-lib/          Core library — auth, launch, license, library, LSX, RTM,
                     OOA, cloudsync. All other crates depend on this.
maxima-cli/          CLI frontend — `maxima-cli launch <offer_id>`, login,
                     listGames, getGameBySlug, cloudSync, etc. Entry point
                     invoked by maxima-bootstrap.
maxima-bootstrap/    Protocol handler binary — registered for link2ea://,
                     origin2://, qrc:// in Wine's registry. Parses the URL,
                     validates the offer_id, and shells out to maxima-cli.
maxima-service/      Windows background service — registry setup, DLL
                     injection for KYBER. Windows-only (no-op `main` on
                     other targets). Not exercised in the Draconis flow.
maxima-tui/          Terminal UI (upstream, ratatui-based). Not built by
                     this fork's CI; preserved for upstream compat.
maxima-ui/           Graphical UI (upstream, eframe/egui). Not built by
                     this fork's CI; preserved for upstream compat.
maxima-resources/    Shared assets — logo, translations.
MaximaHelper/        Native macOS Swift app (build.sh + Info.plist +
                     Sources/main.swift). Bridges qrc:// from the host
                     browser into the bottle via http://127.0.0.1:31033.
installer/           NSIS script (maxima-setup.nsi) + cross-build script
                     (build.sh, uses mingw-w64 + makensis).
images/              Repo images — banners, screenshots.
.github/workflows/   build-ci.yml (push CI), release.yml (tag release),
                     block-upstream-pr.yml (prevent accidental PRs to
                     upstream).
```

Key entry points to know:

| File                                          | What it does                                              |
|-----------------------------------------------|-----------------------------------------------------------|
| `maxima-cli/src/main.rs`                      | CLI argparse + subcommand dispatch                        |
| `maxima-bootstrap/src/main.rs`                | Protocol URL parser + validator + `maxima-cli launch` invocation |
| `maxima-lib/src/core/launch.rs`               | `start_game()` — license, cloud sync, env vars, spawn     |
| `maxima-lib/src/core/auth/login.rs`           | OAuth flow + `remid`-cookie fallback                      |
| `maxima-lib/src/lsx/request/license.rs`       | Denuvo token fetch (env override: `MAXIMA_DENUVO_TOKEN`)  |
| `maxima-lib/src/util/registry.rs`             | Windows registry: install check + protocol registration   |
| `maxima-lib/src/unix/wine.rs`                 | Wine detection, registry setup via `regedit /S`           |
| `maxima-lib/src/util/dll_injector.rs`         | KYBER DLL injection (Windows-only)                        |
| `MaximaHelper/Sources/main.swift`             | NSApplicationDelegate that handles `qrc://` URLs          |
| `installer/maxima-setup.nsi`                  | NSIS script, takes `/DBIN_DIR` for binary location        |

---

## Deltas vs upstream (`ArmchairDevelopers/Maxima`)

Cumulative summary of everything this fork carries on top of upstream `master`. Use this to understand what's macOS/Draconis-specific vs. plain bug fixes that could be sent upstream.

### Infrastructure (macOS/Draconis-specific)

- **`MaximaHelper/`** — new native Swift macOS background agent. Replaces the upstream AppleScript "helper" with a properly bundle-signable binary that LaunchServices will honor for `qrc://`. Universal binary (arm64 + x86_64), built with `swiftc` from `MaximaHelper/build.sh`. Bundle id `com.armchairdevelopers.maxima.helper`, listens for `qrc://` and forwards to `http://127.0.0.1:31033/auth?...` inside the bottle.
- **`installer/maxima-setup.nsi` + `installer/build.sh`** — NSIS-based Windows installer that drops `maxima-cli.exe`, `maxima-bootstrap.exe`, `maxima-service.exe` into the bottle, registers `link2ea://`, `origin2://`, `qrc://` in Wine's registry, and adds start menu shortcuts. Cross-compiled on macOS via `mingw-w64` + `nsis`. Supports `/DBIN_DIR=<path>` override to point at any cargo target dir.
- **`.github/workflows/release.yml`** — three-job release pipeline (macOS builds the helper, Windows builds the installer, Ubuntu collects artifacts and creates the GitHub release). Triggered on `v*` tags.
- **`.github/workflows/build-ci.yml`** — push CI matrix (Linux/Windows/macOS) running the build + sanity checks on every branch.
- **`.github/workflows/block-upstream-pr.yml`** — fires on `pull_request_target` and fails if anyone tries to open a PR against upstream from this fork by accident.

### Code changes (could be sent upstream)

- **Bootstrap protocol-handler hardening** (`maxima-bootstrap/src/main.rs`)
  - `link2ea://` and `origin2://` validate the offer_id against `Origin.OFR.<digits>.<digits>` before invoking `maxima-cli`. Without this, a crafted URL like `link2ea://launchgame/--login=stolen_token` would have made `maxima-cli` interpret `--login` as a flag and bypass OAuth. **(Security)**
  - `origin2://` now reads the real `offerIds` from the URL instead of the hardcoded `Origin.OFR.50.0002148` upstream had. Works for any EA title. **(Bug)**
  - `qrc://` handler no longer panics on URLs missing `login_successful.html?` (was indexing `[1]` on a split vec without bounds checking). **(Bug)**
  - `link2ea://` forwards `KYBER_INTERFACE_PORT` from the parent environment instead of hardcoding `3005`. **(Bug)**
- **`maxima-cli launch` Steam-only owner passthrough** (`maxima-cli/src/main.rs`) — if EA library lookup fails but the slug already matches the EA offer ID pattern, pass it through with a warning instead of bailing. Lets Steam-only TF2 owners launch without linking accounts. **(Bug/UX)**
- **`maxima-cli` `GetGameBySlug` subcommand was a no-op stub** — now actually prints slug/offer_id/content_id/display_name/installed. **(Bug)**
- **`maxima-cli` exhaustive library lookup** — beyond `base_slug` and `base_offer`, scans every game's `slug`/`offer_id`/`product.id`/`product.origin_offer_id`/`offer.content_id`/`product.product.id`. **(Feature, brought in by upstream `9437bcd`.)**
- **DLL injector wide-string support** (`maxima-lib/src/util/dll_injector.rs`) — switched `GetModuleHandleA`/`LoadLibraryA` to `GetModuleHandleW`/`LoadLibraryW` with UTF-16. Fixes injection on non-ASCII install paths. **(Bug, equivalent to upstream `fix/non-ascii-characters` branch.)**
- **Wine registry setup** (`maxima-lib/src/unix/wine.rs`)
  - Added `HKEY_LOCAL_MACHINE\Software\Origin` bare key (some EA titles check this path without the `Electronic Arts\` prefix).
  - `regedit` runs with `/S` (silent) — no longer blocks on a confirmation dialog in Wine.
  - stderr is now piped **and** read, so Wine errors surface in `WineError::Command` instead of being swallowed. **(Bug, partial subset of upstream `fix/wine-registry-setup` branch — the part of that branch that *disabled* `link2ea`/`origin2` protocol registration was intentionally NOT taken because this fork needs them.)**
- **License env override** (`maxima-lib/src/lsx/request/license.rs`) — `MAXIMA_DENUVO_TOKEN` env var short-circuits the license request and returns the token directly. Useful for offline debugging. **(Feature, from upstream `feat/license-token-override`.)**
- **License-update parity** (`maxima-lib/src/core/launch.rs`) — `OnlineOffline` mode now calls `needs_license_update()` before re-requesting, matching `Online` mode. Avoids redundant license server hits. **(Bug, from upstream `fix/license-update-online-offline`.)**

### Removals

- The original AppleScript-based macOS helper. Replaced by the Swift `MaximaHelper.app` above.

---

## CI

Two workflows. Both use **Rust nightly** (required by `#![feature(slice_pattern)]` in `maxima-ui/src/main.rs` and similar feature gates elsewhere — this is inherited from upstream).

### `build-ci.yml` — push CI

Fires on every push to any branch except `v*` tags. Matrix: Linux, Windows, macOS.

| Job             | What it builds                                                                 |
|-----------------|-------------------------------------------------------------------------------|
| ubuntu-latest   | `cargo build --release --target x86_64-unknown-linux-musl -p maxima-cli -p maxima-bootstrap` |
| windows-latest  | `cargo build --release -p maxima-cli -p maxima-bootstrap -p maxima-service`, then `makensis /DBIN_DIR="..\target\release"` |
| macos-latest    | `bash MaximaHelper/build.sh --output ./dist --no-register`, then sanity check that the bundle layout is healthy and `Info.plist` declares `qrc://` |

What CI does **not** validate:

- `maxima-ui` and `maxima-tui` — they pull `rustix 0.37.28` (via `accesskit_unix → zbus 3 → async-process 1.8 → async-io 1.13`) which doesn't build on modern nightly because of the `rustc_attrs` namespace reservation. Excluded from CI to keep the workflow green; the crates themselves are unchanged from upstream.
- `MaximaSetup.exe` actually installing anything in a Wine bottle. We sanity-check size (>100KB) but never run it.
- `MaximaHelper.app`'s code signature — the helper is shipped linker-signed (adhoc) and Draconis re-signs it at consumption time with `codesign --force --deep --sign -` to seal the Info.plist. See "Signing gotcha" below.

### `release.yml` — tag release

Fires on `v*` tags or `workflow_dispatch`. Three jobs:

1. **`build-helper`** (macOS) — builds `MaximaHelper.app`, sanity-checks layout + Info.plist, zips with `--symlinks`, uploads `MaximaHelper.zip` artifact.
2. **`build-installer`** (Windows) — builds the three Draconis-relevant crates, runs `makensis`, sanity-checks installer size >100KB, uploads `MaximaSetup.exe` + a separate `maxima-binaries-win64` artifact with the loose `.exe`s.
3. **`release`** (Ubuntu) — downloads both artifacts and creates a non-prerelease GitHub release. Asset names are fixed: `MaximaHelper.zip` and `MaximaSetup.exe` (Draconis hardcodes these names in `Scripts/fetch-maxima-helper.sh` and `MaximaService.downloadAndInstall`, so do not rename).

### `block-upstream-pr.yml`

Trivial guard that fires on `pull_request_target` and fails if the PR base is `ArmchairDevelopers/Maxima`. Prevents accidentally sending fork-specific changes upstream.

---

## End-to-end launch flow (the one that works)

This is the **only** launch path that works on a Steam-owned, Wine-bottled TF2:

```
1. User clicks Launch in Draconis (vanilla mode)
2. Draconis runs Titanfall2.exe directly via the backend driver
   (CrossOver: cxstart --bottle X Titanfall2.exe).
   For Northstar mode Draconis runs steam.exe -applaunch 1237970 -northstar
   instead (NorthstarLauncher.exe is broken — see below).
3. Titanfall2.exe wants EA auth, emits a URL:
     link2ea://launchgame/Origin.OFR.50.0002694?platform=PCWIN&theme=...
4. Wine routes link2ea:// to maxima-bootstrap.exe (registered by MaximaSetup).
5. maxima-bootstrap parses the URL, takes segments[0] as the offer_id
   (e.g. "Origin.OFR.50.0002694"), and shells out:
     maxima-cli.exe launch <offer_id>
6. maxima-cli authenticates with EA. If the user's EA login needs the
   browser OAuth redirect, EA redirects to a qrc:// URL. The bottle's
   browser hands it to macOS, where MaximaHelper.app is the registered
   handler for qrc:// — it forwards the query back into the bottle by
   hitting http://127.0.0.1:31033/auth?<query>. (maxima-service listens
   there.)
7. maxima-cli resolves the license for the offer_id and TF2 gets its
   auth ticket. Game runs.
```

For Northstar mode the same auth chain still applies after Steam launches the game.

---

## Why NorthstarLauncher.exe is *not* in the flow

`NorthstarLauncher.exe` in the TF2 directory **hard-codes a Win32 attempt to start Origin** (via a path to `Origin.exe`, not via `origin2://`). On macOS / Wine there is no Origin install, and our `origin2://` handler doesn't get a chance to intercept. Result: `[*] Starting Origin... [*] Waiting for Origin...` hangs forever.

Draconis works around this by launching Northstar mode via Steam's `-northstar` launch option (`steam.exe -applaunch 1237970 -northstar`), so Steam invokes `Titanfall2.exe` with the flag and Northstar's `wsock32` proxy hooks load. NorthstarLauncher.exe is never invoked.

If you want to fix Northstar to work standalone here, the right place is to make Northstar's "start Origin" step use `origin2://` (so maxima-bootstrap can catch it). That's an upstream Northstar issue, not Maxima's.

---

## maxima-cli launch — Steam-only owner passthrough (FIXED)

`maxima-cli launch <slug>` looks up the slug against the user's owned EA library before calling the license server:

```rust
// maxima-cli/src/main.rs — Mode::Launch block
// Tries: base_slug, base_offer, then exhaustive match across all known ID fields.
// If nothing matches AND the slug looks like a valid EA offer ID (Origin.OFR.X.Y),
// passes it through directly with a warning and lets EA's license server decide.
// Otherwise bails with a descriptive error pointing to https://www.ea.com.
```

**Previously broken for Steam-only owners** whose EA account doesn't have TF2 linked — `maxima-cli` would log in fine but bail with `"No owned offer found for 'Origin.OFR.50.0002694'"`. This is now fixed: if the slug matches `Origin\.OFR\.\d+\.\d+` and the library lookup fails, it falls through with a warning instead of an error.

The user-side fix (linking Steam ↔ EA at https://www.ea.com) still removes the warning and is recommended for full LSX functionality.

There's also `--login <token>` mode (`maxima-cli launch <content_id> --login ...`) which treats the slug as a content id and skips the library lookup entirely — but it disables online LSX and uses a dummy persona name.

Stale Draconis releases (≤ v0.3.9) called `maxima-cli launch 1237970` directly, where `1237970` is the *Steam* app id, not an EA slug — the library lookup obviously didn't match anything. v0.4.0 of Draconis stopped doing this: the only path that reaches `maxima-cli` is via `link2ea://`, where the slug is the real EA offer id.

---

## URI protocols Maxima owns

| Scheme       | Registered by        | Where         | Handler does                                             |
|--------------|----------------------|---------------|----------------------------------------------------------|
| `qrc://`     | `MaximaHelper.app`   | macOS host    | GETs `http://127.0.0.1:31033/auth?<query>` inside bottle |
| `qrc://`     | maxima-bootstrap.exe | Wine registry | same target (host handler is preferred when Draconis runs)|
| `link2ea://` | maxima-bootstrap.exe | Wine registry | extracts offer_id, runs `maxima-cli launch <offer_id>`   |
| `origin2://` | maxima-bootstrap.exe | Wine registry | extracts real `offerIds` from URL, runs `maxima-cli launch <offer_id>` |

**Note on `origin2://`:** The upstream handler hardcoded `Origin.OFR.50.0002148` (Star Wars Battlefront 2). This fork now reads the `offerIds` query parameter from the URL and uses that, making `origin2://` generic across all EA games.

MaximaHelper.app's bundle id is `com.armchairdevelopers.maxima.helper`. **The Draconis fork's Info.plist must remain signed-sealed** — see signing issue below.

---

## Signing gotcha (relevant when packaging MaximaHelper)

The upstream zipped `MaximaHelper.app` is shipped **linker-signed only**:

```
codesign -dv MaximaHelper.app
  CodeDirectory ... flags=0x20002(adhoc,linker-signed)
  Info.plist=not bound
  Sealed Resources=none
  Identifier=MaximaHelper_arm64                    ← not the real CFBundleIdentifier
```

LaunchServices **silently refuses to honor URL handler claims** from a bundle whose Info.plist isn't sealed into the signature. Draconis fixes this at build time by re-signing the cached helper:

```bash
codesign --force --deep --sign - MaximaHelper.app
# → Identifier=com.armchairdevelopers.maxima.helper
# → Info.plist entries=13, Sealed Resources files=1
```

If you ever change how `MaximaHelper.app` is signed at release time in this repo, make sure the final artifact is properly bundle-signed (not just linker-signed), or downstream `NSWorkspace.setDefaultApplication(at:toOpenURLsWithScheme: "qrc")` will silently no-op and `qrc://` will stay bound to whatever was registered before.

---

## EA identifiers cheat sheet

| Thing                     | TF2 value                               |
|---------------------------|-----------------------------------------|
| Steam App ID              | `1237970` (Steam-only — do **not** pass to maxima-cli) |
| EA Origin offer id        | `Origin.OFR.50.0002694` (extracted from link2ea://)     |
| MaximaHelper bundle id    | `com.armchairdevelopers.maxima.helper`  |
| MaximaHelper qrc port     | `127.0.0.1:31033` inside Wine            |

---

## Diagnostics

### Is the helper registered for qrc:// on the host?

```bash
swift -e 'import AppKit; let u = URL(string: "qrc://probe")!; \
  print(NSWorkspace.shared.urlForApplication(toOpen: u)?.path ?? "NONE")'
```

Should print `/Applications/Draconis.app/Contents/Resources/MaximaHelper.app`. If not, Draconis's `registerHelper()` failed or another bundle is winning.

### Is the helper signature healthy?

```bash
codesign -dv /Applications/Draconis.app/Contents/Resources/MaximaHelper.app 2>&1 \
  | grep -E '(Identifier|Info.plist|Sealed Resources)'
```

Want to see `Identifier=com.armchairdevelopers.maxima.helper`, `Info.plist entries=13`, `Sealed Resources version=2`. If it says `Identifier=MaximaHelper_arm64` or `Info.plist=not bound`, the helper wasn't re-signed.

### Are there stale helper copies LS knows about?

`mdfind 'kMDItemCFBundleIdentifier == "com.armchairdevelopers.maxima.helper"'` only sees indexed paths. For the full LS view:

```bash
LSREG=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
"$LSREG" -dump | awk '
  /^-{20}/{block=""; next}
  {block=block $0 "\n"}
  /claimed schemes:.*qrc:/{matches=matches block}
  END{print matches}
' | grep '^path:'
```

Common offenders: mounted `Draconis-vX.dmg` (`/Volumes/Draconis [N]/...`), Xcode `DerivedData/Draconis-*/Build/Products/Debug/Draconis.app`, ad-hoc unzips in `/private/tmp/MaximaHelper.app`. Draconis v0.3.7+ auto-unregisters these via `NSWorkspace.urlsForApplications(withBundleIdentifier:)` before calling `setDefaultApplication`.

### Is maxima-bootstrap actually being invoked?

Inside the bottle, maxima-bootstrap appends to `%TEMP%/maxima_execution.log` on every invocation (see `maxima-bootstrap/src/main.rs`). On a CrossOver bottle that's typically `~/Library/Application Support/CrossOver/Bottles/<bottle>/drive_c/users/crossover/Temp/maxima_execution.log`. If this file isn't growing when a launch is attempted, the protocol handler registration is broken and TF2's `link2ea://` is going nowhere.

### Steam vs vanilla launch contract (Draconis ↔ here)

Draconis v0.4.0+:

- Vanilla launch: runs `Titanfall2.exe` directly. The binary's own Steam DRM stub self-relaunches via `steam://run/1237970` if needed; the EA path triggers `link2ea://` which reaches maxima-bootstrap.
- Northstar launch: runs `steam.exe -applaunch 1237970 -novid -northstar`. Steam routes through TF2, the Northstar hooks load, EA auth still goes via link2ea:// → maxima-bootstrap → maxima-cli.

Draconis never calls `maxima-cli.exe` directly anymore. If you see `maxima-cli launch 1237970` in any log, it's from an old Draconis (≤ v0.3.9).

---

## Release flow for this repo

Draconis pulls the latest release of this fork at build time via `Scripts/fetch-maxima-helper.sh`:

```
GET https://api.github.com/repos/AA-EION/Maxima-Draconis/releases/latest
→ download MaximaHelper.zip asset
→ unzip into Draconis/Resources/MaximaHelper.app
→ codesign --force --deep --sign - to seal the Info.plist
→ xcodegen + xcodebuild bundles it into Draconis.app
```

So a new MaximaHelper release flows into the next Draconis build automatically as long as the asset is named `MaximaHelper.zip` and `MaximaSetup.exe` (for the in-bottle installer).

Tag the release as `vX.Y.Z` (lowercase v). The bottle installer is downloaded by Draconis on demand via `MaximaService.downloadAndInstall` — it fetches the latest release's `MaximaSetup.exe`, copies it into the bottle's `drive_c/windows/Temp/`, runs it silently with `/S`.

---

## Working on this repo

```bash
bash MaximaHelper/build.sh           # build the macOS helper
bash installer/build.sh              # cross-compile MaximaSetup.exe (mingw + nsis)
cargo build --release --target x86_64-pc-windows-gnu -p maxima-cli
cargo build --release --target x86_64-pc-windows-gnu -p maxima-bootstrap
cargo build --release --target x86_64-pc-windows-gnu -p maxima-service
```

Anything that affects the Draconis integration — protocol handler registration, offer_id resolution, Info.plist contents in MaximaHelper, `MaximaSetup.exe`'s install location — is worth flagging in the release notes so Draconis can adapt.

---

## Upstream branch survey (as of 2026-05-14)

Evaluated all 14 upstream branches. Only these were complete and merged-ready:

| Branch | Status in this fork |
|--------|---------------------|
| `feat/license-token-override` | ✅ Already merged (commit `6ab4631`) |
| `fix/license-update-online-offline` | ✅ Already merged (commit `246bc53`) |
| `fix/non-ascii-characters` | ✅ Applied in this session |
| `fix/wine-registry-setup` | ✅ Partially applied (registry additions + silent regedit; the part that disabled link2ea/origin2 was intentionally skipped) |

The remaining branches (`feat/server`, `feature/umu-launcher`, `feat/new-ci`, etc.) are either stale (6–20 months old), have unresolved conflicts, or are WIP with no clear completion signal. Do not merge them without a full review.

---

## Changelog

### Session 2026-05-14 (CI + release pipeline)

#### Fixed — `.github/workflows/build-ci.yml`
CI had been red on every commit since the workflow was added (5+ master pushes, none green). Two unrelated breakages:
- **Linux**: `cargo build --release` built the whole workspace, which pulled `maxima-ui → eframe → egui-winit → accesskit_winit → accesskit_unix → zbus 3.15 → async-process 1.8 → async-io 1.13 → rustix 0.37.28`. Recent nightlies (1.97.0-nightly) reserved the `rustc_*` attribute namespace, so rustix 0.37 fails to compile. Restricted Linux to `-p maxima-cli -p maxima-bootstrap`, which only pull rustix 0.38/1.x via tempfile/tokio. Also dropped the X11/xkbcommon apt deps that were only needed by `maxima-ui`. **Cross-OS impact**: none — the excluded crates still live in the workspace and a downstream user on a working toolchain can still `cargo build -p maxima-ui` locally.
- **Windows**: NSIS script defaulted to `${BIN_DIR}=..\target\x86_64-pc-windows-gnu\release\` but the runner compiles MSVC (`target/release/`). Passed `/DBIN_DIR="..\target\release"` through. Also restricted to the three Draconis-relevant crates.

#### Added — `.github/workflows/build-ci.yml` macOS job
Added a third matrix entry that builds `MaximaHelper.app` via `MaximaHelper/build.sh` and validates the bundle layout + that `Info.plist` declares `CFBundleURLTypes` with `qrc://`. Catches helper regressions on every PR instead of only at tag time. Rust/protoc/rust-cache steps are gated `if: runner.os != 'macOS'` so the macOS job stays a pure Swift build.

#### Fixed — `.github/workflows/release.yml`
The release pipeline had the same `cargo build --release` problem and would have failed silently on the next tag. Restricted the Windows job to the Draconis-relevant crates, added installer-size sanity check, added helper-bundle sanity check, and now uploads loose Windows binaries as a separate artifact (debug aid).

### Session 2026-05-14 (code fixes)

#### Fixed — `maxima-lib/src/util/dll_injector.rs`
DLL injection broke on non-ASCII installation paths (e.g. usernames or bottle paths with accented characters). Root cause: `GetModuleHandleA` / `LoadLibraryA` only accept ANSI strings. Fixed by switching to `GetModuleHandleW` / `LoadLibraryW` with UTF-16 wide strings, matching the `fix/non-ascii-characters` upstream branch. **Cross-OS impact**: file is Windows-only (`use winapi::...`); change benefits native Windows users equally.

#### Fixed — `maxima-lib/src/unix/wine.rs`
Two issues in `setup_wine_registry()`:
1. Missing `HKEY_LOCAL_MACHINE\Software\Origin` bare key — some games check for this path without the `Electronic Arts\` prefix and would fail to recognise Origin as installed.
2. `regedit` was called without the `/S` (silent) flag, causing it to show a confirmation dialog that blocked the launch flow silently in Wine. Also added `Stdio::piped()` for stderr and `read+append` for stderr-to-`output_str`, so Wine errors surface in `WineError::Command` instead of disappearing.

**Cross-OS impact**: file is unix-only (`#[cfg(unix)]`). The change benefits Linux + macOS/CrossOver equally; native Windows doesn't compile this file.

#### Fixed — `maxima-bootstrap/src/main.rs`
The `origin2://` protocol handler had `Origin.OFR.50.0002148` (Star Wars Battlefront 2) hardcoded, making it useless for any other game. Also used wrong CLI syntax (`--mode launch --offer-id X` doesn't exist in this version of maxima-cli). Fixed to read the real `offerIds` from the URL query string and call `maxima-cli launch <offer_id>`. The handler now works generically for any EA title that emits `origin2://`. **Cross-OS impact**: code is portable (no `#[cfg]` gates); benefits Linux/Windows users of maxima-bootstrap who register `origin2://` natively.

#### Fixed — `maxima-cli/src/main.rs`
`maxima-cli launch Origin.OFR.X.Y` would bail with `"No owned offer found"` for Steam-only owners whose EA library is empty (TF2 not linked). Added offer_id passthrough: if all library lookups fail but the slug matches the `Origin.OFR.\d+\.\d+` pattern, Maxima passes it directly to the license server with a warning. Users are directed to link accounts at https://www.ea.com for the cleanest experience. **Cross-OS impact**: portable code; benefits any platform.

#### Fixed — `maxima-cli/src/main.rs`
`GetGameBySlug` subcommand was a no-op stub (body commented out, returned `Ok(())`). Now prints slug, offer ID, content ID, display name, and installed status.

#### Security — `maxima-bootstrap/src/main.rs` (CLI flag injection)
The `link2ea://` and `origin2://` handlers passed the URL-derived `offer_id` directly as a positional argument to `maxima-cli launch`. Because that segment comes from an attacker-controlled URL, a crafted link like `link2ea://launchgame/--login=stolen_token?platform=PCWIN` would have made `maxima-cli` interpret `--login` as a real flag (it exists in the CLI and treats the value as an EA access token, bypassing the OAuth flow). Added `is_valid_ea_offer_id()` strict validation (`Origin.OFR.<digits>.<digits>`) before invoking the subprocess. Rejects log to `maxima_execution.log` for diagnostics.

#### Fixed — `maxima-bootstrap/src/main.rs` (panic in qrc:// handler)
The `qrc://` handler did `arg.split("login_successful.html?").collect::<Vec<&str>>()[1]` — indexing `[1]` panics if the marker is absent. Replaced with `splitn(2, ...).get(1)` and a graceful early return.

---

## Known remaining gaps

- **`maxima-tui` / `maxima-ui`**: The UI crates exist and compile but are not wired into the Draconis flow at all. They are upstream components that may be useful in a future Draconis "standalone mode" but need significant work to be production-ready.
- **`origin2://` without an `offerIds` param**: If the URL has no `offerIds` the handler now passes an empty string to `maxima-cli`, which will fail gracefully but not helpfully. A better fallback (e.g. reading from query params `productId` or hardcoding a per-game table) is a future improvement.
- **DLL injection on macOS / CrossOver**: `maxima-service`'s DLL injector is Windows-only by design. CrossOver / Wine does not support `CreateRemoteThread` injection. The service is installed by the NSIS installer but its injection path is never exercised in the Draconis flow.
- **Cloud saves, downloads, friends**: All implemented upstream and present in the codebase, but untested in the Draconis / CrossOver configuration.
- **Offline mode after first launch**: The `LaunchMode::Offline` path exists but Draconis does not yet expose it in the UI. License cache lives at `C:/ProgramData/Maxima/Licenses/<content_id>.dlf` and is valid for approximately two weeks.
