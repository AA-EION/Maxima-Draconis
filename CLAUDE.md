# Maxima-Draconis — engineering reference for Claude agents

This is the **Maxima-Draconis fork** — the EA authentication and launch backend used by [Draconis](https://github.com/AA-EION/Draconis), a native macOS launcher for Titanfall 2 on CrossOver / Wine. This file is the living engineering reference for anyone picking up the repo cold. It covers architecture, known gotchas, diagnostics, and a running changelog.

---

## What Maxima is

Open-source replacement for the EA Desktop Launcher. **Not** a macOS-native app — `maxima-cli` / `maxima-bootstrap` / `maxima-service` are Windows binaries that run **inside the CrossOver bottle** alongside Titanfall 2. The only piece that runs on the macOS host is `MaximaHelper.app`, a tiny Swift background agent that bridges EA's `qrc://` OAuth redirect from the user's browser into the bottle.

The Draconis fork is tested *only* for Titanfall 2 on macOS via CrossOver. Other configurations may work but aren't supported here.

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

### Session 2026-05-14

#### Fixed — `maxima-lib/src/util/dll_injector.rs`
DLL injection broke on non-ASCII installation paths (e.g. usernames or bottle paths with accented characters). Root cause: `GetModuleHandleA` / `LoadLibraryA` only accept ANSI strings. Fixed by switching to `GetModuleHandleW` / `LoadLibraryW` with UTF-16 wide strings, matching the `fix/non-ascii-characters` upstream branch.

#### Fixed — `maxima-lib/src/unix/wine.rs`
Two issues in `setup_wine_registry()`:
1. Missing `HKEY_LOCAL_MACHINE\Software\Origin` bare key — some games check for this path without the `Electronic Arts\` prefix and would fail to recognise Origin as installed.
2. `regedit` was called without the `/S` (silent) flag, causing it to show a confirmation dialog that blocked the launch flow silently in Wine. Also added `Stdio::piped()` for stderr so Wine errors surface in logs instead of disappearing.

#### Fixed — `maxima-bootstrap/src/main.rs`
The `origin2://` protocol handler had `Origin.OFR.50.0002148` (Star Wars Battlefront 2) hardcoded, making it useless for any other game. Also used wrong CLI syntax (`--mode launch --offer-id X` doesn't exist in this version of maxima-cli). Fixed to read the real `offerIds` from the URL query string and call `maxima-cli launch <offer_id>`. The handler now works generically for any EA title that emits `origin2://`.

#### Fixed — `maxima-cli/src/main.rs`
`maxima-cli launch Origin.OFR.X.Y` would bail with `"No owned offer found"` for Steam-only owners whose EA library is empty (TF2 not linked). Added offer_id passthrough: if all library lookups fail but the slug matches the `Origin.OFR.\d+\.\d+` pattern, Maxima passes it directly to the license server with a warning. Users are directed to link accounts at https://www.ea.com for the cleanest experience.

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
