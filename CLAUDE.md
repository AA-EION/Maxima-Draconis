# Maxima-Draconis — guidance for Claude agents

This is the **Maxima-Draconis fork** — the EA authentication and launch backend used by [Draconis](https://github.com/AA-EION/Draconis), a native macOS launcher for Titanfall 2 on CrossOver / Wine. This file captures the architecture, the surrounding system Draconis integrates with, and the diagnostic context that has accumulated while wiring the two together. Read it before touching anything if you're picking up cold.

## What Maxima is

Open-source replacement for the EA Desktop Launcher. **Not** a macOS-native app — `maxima-cli` / `maxima-bootstrap` / `maxima-service` are Windows binaries that run **inside the CrossOver bottle** alongside Titanfall 2. The only piece that runs on the macOS host is `MaximaHelper.app`, a tiny Swift agent that bridges EA's `qrc://` OAuth redirect from the user's browser into the bottle.

The Draconis fork is tested *only* for Titanfall 2 on macOS via CrossOver. Other configurations may work but aren't supported here.

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
        ├── maxima-service.exe     — background service
        └── Uninstall.exe          — NSIS uninstaller from MaximaSetup.exe
```

Build outputs:

- `installer/MaximaSetup.exe` — NSIS bundle that installs everything in the bottle and registers the protocol handlers in Wine's registry. Cross-compiled on macOS via `mingw-w64` + `nsis`.
- `MaximaHelper/build/MaximaHelper.app` — built on macOS with Xcode CLT.
- `MaximaHelper.zip` — release asset Draconis downloads at build time.

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

## Why NorthstarLauncher.exe is *not* in the flow

`NorthstarLauncher.exe` in the TF2 directory **hard-codes a Win32 attempt to start Origin** (via a path to `Origin.exe`, not via `origin2://`). On macOS / Wine there is no Origin install, and our `origin2://` handler doesn't get a chance to intercept. Result: `[*] Starting Origin... [*] Waiting for Origin...` hangs forever.

Draconis works around this by launching Northstar mode via Steam's `-northstar` launch option (`steam.exe -applaunch 1237970 -northstar`), so Steam invokes `Titanfall2.exe` with the flag and Northstar's `wsock32` proxy hooks load. NorthstarLauncher.exe is never invoked.

If you want to fix Northstar to work standalone here, the right place is to make Northstar's "start Origin" step use `origin2://` (so maxima-bootstrap can catch it). That's an upstream Northstar issue, not Maxima's.

## maxima-cli launch — known limitation

`maxima-cli launch <slug>` (the command maxima-bootstrap calls) looks up the slug against the user's owned EA library:

```rust
// maxima-cli/src/main.rs around line 285–321
// Tries: base_slug, base_offer, then exhaustive match across
//   slug / offer_id / product.id / product.origin_offer_id /
//   offer.content_id / product.product.id
// Bails with "No owned offer found for '<slug>'" if none match.
```

**This fails for Steam-only owners** whose EA account doesn't have TF2 linked. The user logs into EA fine (`Logged in as XNovaDelta!`) but their EA library is empty for TF2, so the lookup never matches.

Two ways forward:

- **User-side workaround**: link Steam ↔ EA account at https://www.ea.com → TF2 appears in the EA library → lookup succeeds.
- **Maxima fix**: when the lookup fails and the slug already looks like a well-formed offer id (`Origin\.OFR\.\d+\.\d+` for example), pass it through to `LaunchMode::Online(offer_id)` anyway and let EA's license server decide. The risk is online LSX features may fail without ownership. Worth scoping if Draconis users keep hitting this.

There's also `--login <token>` mode (`maxima-cli launch <content_id> --login ...`) which treats the slug as a content id and skips the library lookup — but it disables online LSX and uses a dummy persona name.

Stale Draconis releases (≤ v0.3.9) called `maxima-cli launch 1237970` directly, where `1237970` is the *Steam* app id, not an EA slug — the library lookup obviously didn't match anything. v0.4.0 of Draconis stopped doing this: the only path that reaches `maxima-cli` is via `link2ea://`, where the slug is the real EA offer id.

## URI protocols Maxima owns

| Scheme         | Registered by         | Where        | Handler does                                              |
|----------------|----------------------|--------------|-----------------------------------------------------------|
| `qrc://`       | `MaximaHelper.app`    | macOS host   | GETs `http://127.0.0.1:31033/auth?<query>` inside bottle  |
| `qrc://`       | maxima-bootstrap.exe  | Wine registry| same target (host handler is preferred when Draconis runs)|
| `link2ea://`   | maxima-bootstrap.exe  | Wine registry| extracts offer_id, runs `maxima-cli launch <offer_id>`    |
| `origin2://`   | maxima-bootstrap.exe  | Wine registry| extracts cmdParams, runs `maxima-cli` with hardcoded offer|

MaximaHelper.app's bundle id is `com.armchairdevelopers.maxima.helper`. **The Draconis fork's Info.plist must remain signed-sealed** — see signing issue below.

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

## EA identifiers cheat sheet

| Thing                     | TF2 value                               |
|---------------------------|-----------------------------------------|
| Steam App ID              | `1237970` (Steam-only — do **not** pass to maxima-cli) |
| EA Origin offer id        | `Origin.OFR.50.0002694` (extracted from link2ea://)     |
| MaximaHelper bundle id    | `com.armchairdevelopers.maxima.helper`  |
| MaximaHelper qrc port     | `127.0.0.1:31033` inside Wine            |

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

## Working on this repo

```bash
bash MaximaHelper/build.sh           # build the macOS helper
bash installer/build.sh              # cross-compile MaximaSetup.exe (mingw + nsis)
cargo build --release --target x86_64-pc-windows-gnu -p maxima-cli
cargo build --release --target x86_64-pc-windows-gnu -p maxima-bootstrap
```

Anything that affects the Draconis integration — protocol handler registration, offer_id resolution, Info.plist contents in MaximaHelper, `MaximaSetup.exe`'s install location — is worth flagging in the release notes so Draconis can adapt.
