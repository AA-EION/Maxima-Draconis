# Bundling Maxima on macOS + the server-owned status icon

Two questions this answers:

1. **Where does `maxima-server` live** so it can be registered (launchd) and
   spawned by clients / games / other frontends?
2. **How does the server own a status-bar icon on all three OSes**, with a menu
   to open the UI or stop the server?

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

## 4. Logo placement summary

| Surface | Asset | Where set |
|---|---|---|
| macOS launcher (Dock / window / Finder) | `logo.png` → `AppIcon.icns` | `Maxima.app` Info.plist `CFBundleIconFile` |
| macOS status bar (the server's presence) | M-in-circle template | `maximaStatusIcon()` in the app |
| Windows tray (the server's presence) | `logo.ico` | `tray.rs` `LoadImage` from embedded resource |
| Windows UI window/taskbar | `logo.ico` | `maxima-resources` winres (existing) |
