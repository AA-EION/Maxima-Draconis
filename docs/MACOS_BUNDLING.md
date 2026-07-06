# Bundling Maxima on macOS + the server-owned status icon

Two questions this answers:

1. **Where does `maxima-server` live** so it can be registered (launchd) and
   spawned by clients / games / other frontends?
2. **How does the server own a status-bar icon on all three OSes**, with a menu
   to open the UI or stop the server?

## 0. The server is independent of every GUI

The `maxima-server` process is **not owned by any frontend**. Closing the
SwiftUI app (or the egui UI, or the CLI exiting) must never stop it. Two
mechanisms guarantee this:

- **When a frontend spawns it** (no service installed), it's spawned into its
  **own session** (`posix_spawn` + `POSIX_SPAWN_SETSID` in Swift;
  `setsid()` via `pre_exec` in the Rust paths). A plain `Process`/child stays
  in the app's launchd job and macOS reaps it on quit — that was the "server
  dies when I close the window" bug. SETSID detaches it.
- **When the service is installed** (recommended), **launchd owns it** — no
  frontend spawns it at all.

## 0b. Boot policy — auto / on-demand / manual

`maxima-cli service install --boot <policy>` registers the server with the OS
and records the policy in `config.json` (read by every frontend). The SwiftUI
Settings → *Background service* section is the GUI for the same thing.

| Policy | What it does |
|---|---|
| `auto` | launchd LaunchAgent with `RunAtLoad` — the server starts at login and is always up. |
| `on-demand` *(default)* | No autostart; a frontend or a game launch spawns the (detached) server when it opens, and it keeps running. |
| `manual` | No autostart, no auto-spawn. The user starts it explicitly (Settings "Start Server", or `maxima-server`). |

`maxima-cli service uninstall [--purge]` removes the autostart, the
LaunchServices protocol claims, the installed binaries, and `config.json` —
leaving **no trace** that could interfere with the official EA app. `--purge`
also deletes cached auth tokens + logs. Game bottles are kept (remove them from
CrossOver manually).

## 0c. Why a classic LaunchAgent, not SMAppService

`SMAppService` (macOS 13+) is the modern API, but it **requires a real
Apple-issued signing identity** — ad-hoc / "Sign to Run Locally" signing
[doesn't work with it](https://theevilbit.github.io/posts/smappservice/). Since
this project is self-distributed without a paid cert, Maxima uses a **classic
`launchd` LaunchAgent** in `~/Library/LaunchAgents/` instead: launchd imposes no
signing requirement on user agents, so it works ad-hoc. The agent's
`ProgramArguments` points at the binary in the stable App Support path (below) —
not inside the `.app`, which is unstable under app-translocation.

## 1. Install location — `~/Library/Application Support/Maxima/bin/`

The canonical, registerable copy of the native binaries lives at:

```
~/Library/Application Support/Maxima/bin/
    maxima-server
    maxima-cli
    maxima-bootstrap
    MaximaBootstrap.app/         (bundle-signed protocol handler)
```

**Why here and not inside `Maxima.app/Contents/Resources/`:**

- **App translocation.** A quarantined `.app` (downloaded, not yet "moved") runs
  from a randomized read-only path under `/private/var/folders/…/AppTranslocation/`.
  A launchd plist or a game/bootstrap that hardcoded a path *inside the bundle*
  would point at a location that changes every launch. A stable App Support path
  doesn't move.
- **launchd wants an absolute, stable `ProgramArguments[0]`.** The autostart
  agent references the App-Support path; app updates/moves don't invalidate it.
- **One agreed-upon path for every caller.** The CLI (maybe installed elsewhere),
  the TUI, the SwiftUI app, and `MaximaBootstrap.app` (spawned by the game via
  `link2ea://`) all need to find the *same* server. A well-known path is that
  contract; "wherever the .app happens to be" is not.
- **The `.app` is a *distribution* vehicle, not the runtime home.** `Maxima.app`
  (and any installer) copies the binaries into App Support on first run — a cheap
  idempotent sync — then everything resolves the runtime copy from App Support.

**Resolution order** (both `maxima-cli`'s `locate_server_binary` in Rust and
`MaximaCLI.locateServer()` in Swift implement this):

1. Sibling of the current executable (installer / cargo `target/release` layout).
2. Bundled `Contents/Resources` (when running from inside `Maxima.app`).
3. **`~/Library/Application Support/Maxima/bin/`** (the stable registered copy).
4. `PATH`.

`maxima-server` self-installs its sibling binaries into (3) on startup (best
effort) so that after the very first run from *any* layout, the stable copy
exists for launchd and for game-spawned bootstrap.

## 2. The status icon is the server's, on every OS

The rule the user set: **the server spawns the icon**; the menu can **stop the
server** or **open Maxima** (SwiftUI on macOS, egui on Windows/Linux). Each OS
uses its native idiom, dispatched from one place —
[`maxima-server/src/status_icon.rs`](../maxima-server/src/status_icon.rs),
`status_icon::spawn(port)`, called right after the server binds.

### Windows — native tray, in-process

`Shell_NotifyIcon` on a dedicated thread with a Win32 message loop
([tray.rs](../maxima-server/src/tray.rs)). Icon = `maxima-resources/logo.ico`
(embedded). Menu: **Open Maxima** (`maxima.exe`, the egui UI, beside the server)
· **Stop Server** (sends `{"cmd":"shutdown"}` to the control port). This is the
literal "server draws the icon" case — a headless Windows service can host a
tray directly.

### macOS — server spawns the menu-bar host

macOS status items require a GUI process (an `NSApplication` run loop); a headless
launchd Rust binary can't draw one. So the server **spawns `Maxima.app` in
menu-bar mode** and that process draws the `MenuBarExtra`. This *reuses the
working SwiftUI menu* instead of re-implementing a Cocoa status item from Rust,
and the app doubles as a thin client of the same server.

- `status_icon::spawn` on macOS runs `open -b com.armchairdevelopers.maxima.native
  --args --menubar` (falls back to `open <resolved Maxima.app>`).
- Launched with `--menubar`, the app sets activation policy `.accessory` (no dock
  icon, no auto-opened window) and shows only the `MenuBarExtra`. Its `Backend`
  finds the already-running server and connects — **no spawn loop** (the server
  is up before it launches the app; `open` coalesces to the single running
  instance if the app is already open).
- Menu (same three actions as Windows): **Open Maxima** → promotes to `.regular`
  and opens the SwiftUI window · **Stop Server** → shutdown · **Quit**.
- Icon = the **M-in-a-circle** template mark (solid circle, M knocked out) so the
  menu bar tints it for light/dark automatically. The launcher's own app icon
  (Dock / window) is the full `logo.png` → `AppIcon.icns`.
- If `Maxima.app` isn't installed (CLI-only install), the server logs that it
  can't show a menu-bar icon and stays fully functional — use `server-status`.

The launchd agent launches `maxima-server`, which spawns the menu-bar host — so
the user's "server spawns the icon" holds transitively, and the icon is present
from login without a window flashing up.

### Linux — SNI tray behind an optional feature; headless by default

Linux desktops expose tray icons via the freedesktop **StatusNotifierItem**
D-Bus spec — no GTK required. `maxima-server` implements it with the pure-Rust
`ksni` crate behind the **`linux-tray` cargo feature (off by default)**:

- Default Linux build stays dependency-free and **headless** (the long-standing
  design for servers with no display) — drive it with `maxima-cli` + the systemd
  user unit.
- `cargo build -p maxima-server --features linux-tray` adds the SNI tray: **Open
  Maxima** (`maxima` egui UI on `PATH`) · **Stop Server** · **Quit**. On a host
  with no D-Bus/display the SNI registration simply fails and the server runs on
  headless.

> Honesty note: the `linux-tray` path is compiled behind the feature but is not
> built by the current Linux CI job (which builds only `maxima-cli` +
> `maxima-bootstrap`) and hasn't been runtime-tested on this machine. Treat it as
> "available, verify on a Linux desktop before relying on it."

## 3. Autostart at login

- **macOS** — [installer/autostart/com.armchairdevelopers.maxima.server.plist](../installer/autostart/com.armchairdevelopers.maxima.server.plist),
  `ProgramArguments` = the App-Support `maxima-server`. `launchctl bootstrap
  gui/$UID <plist>` (install instructions in the file header). The server spawns
  the menu-bar host on start.
- **Windows** — NSIS writes `HKCU\…\Run\MaximaServer` → `maxima-server.exe`
  (uninstaller stops + removes it). The server draws its own tray.
- **Linux** — [installer/autostart/maxima-server.service](../installer/autostart/maxima-server.service)
  (systemd *user* unit). Headless unless built with `linux-tray`.

## 3b. Distribution + install / uninstall

Two supported vehicles; both end with the same registered state:

**DMG (drag-install) + first-run registration.** Ship `Maxima.app` in a DMG.
The user drags it to /Applications and opens it; on first run the server
self-syncs its binaries to the App Support path, and **Settings → Background
service** lets them pick a boot policy (which registers the LaunchAgent). This
needs no root. Uninstall: Settings → *Uninstall service*, then trash the app.

**PKG (recommended for "registers everything").** `bash maxima-native/make-pkg.sh`
builds `Maxima-Installer.pkg`, which:
- installs `Maxima.app` into `/Applications` (a pkg-placed app is **not**
  translocated, so its bundled binaries are at a stable path);
- runs a **postinstall** that symlinks `maxima-cli` / `maxima-tui` into
  `/usr/local/bin` (CLI + TUI on `PATH`) and runs
  `maxima-cli service install --boot on-demand` **as the console user** to
  register the server + protocol handlers.

Uninstall from either vehicle is one command — it leaves no trace:

```bash
maxima-cli service uninstall            # agent + protocol claims + binaries + config
maxima-cli service uninstall --purge    # also removes cached tokens + logs
```

`make-pkg.sh` also emits `uninstall.sh` alongside the pkg for users who removed
the CLI first.

## 4. Logo placement summary

| Surface | Asset | Where set |
|---|---|---|
| macOS launcher (Dock / window / Finder) | `logo.png` → `AppIcon.icns` | `Maxima.app` Info.plist `CFBundleIconFile` |
| macOS status bar (the server's presence) | M-in-circle template | `maximaStatusIcon()` in the app |
| Windows tray (the server's presence) | `logo.ico` | `tray.rs` `LoadImage` from embedded resource |
| Windows UI window/taskbar | `logo.ico` | `maxima-resources` winres (existing) |
