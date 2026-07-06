<p align="center">
  <img src="images/1500x500.jpg" alt="Maxima-Draconis banner" />
</p>

<h1 align="center">Maxima-Draconis</h1>

<p align="center">
  EA authentication and launch backend for <a href="https://github.com/AA-EION/Draconis">Draconis</a> — Titanfall 2 on macOS via CrossOver / Wine.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/macOS-native%20%2B%20CrossOver-lightgrey?logo=apple" alt="macOS" />
  <img src="https://img.shields.io/badge/Rust-nightly-F74C00?logo=rust&logoColor=white" alt="Rust nightly" />
  <img src="https://img.shields.io/github/license/ArmchairDevelopers/Maxima?color=blue" alt="GPL-3.0" />
</p>

---

> [!WARNING]
> **Beta — primarily maintained for [Draconis](https://github.com/AA-EION/Draconis) on macOS/CrossOver.** The code stays portable to the other OSes upstream supports (native Linux + Windows) and CI keeps them compiling, but only the macOS/CrossOver path is actively tested. For vanilla Maxima on Linux or native Windows, the [upstream repo](https://github.com/ArmchairDevelopers/Maxima) is a better fit.

**Maxima is an open-source replacement for the EA Desktop / Origin launcher.** It performs the EA authentication handshake and license resolution that EA-published games (here: Titanfall 2) require at startup. Maxima is a *universal* EA launcher — per-title glue (Northstar, mods, TF2 detection) lives in the consumer ([Draconis](https://github.com/AA-EION/Draconis)), built on Maxima's machine-readable primitives.

The deep engineering reference (sequence diagrams, gotchas, changelog) is [`CLAUDE.md`](./CLAUDE.md). This README is the map: **what every part of the app is.**

---

## The core idea: one server, thin clients

Maxima runs as **one background process — `maxima-server` — that does everything**: it holds the logged-in EA session, the LSX auth listener (port 3216), the `/authorize` HTTP endpoint (13219), RTM friends presence, and all downloads/installs/launches. Every user-facing surface (CLI, TUI, the graphical UIs) is a **thin client** that talks to it over a small typed RPC on a loopback socket (13220). This is upstream PR [#23](https://github.com/ArmchairDevelopers/Maxima/pull/23)'s "Maxima Server" design.

```
                         ┌──────────────────────────────┐
   maxima-cli  ─────┐    │        maxima-server         │
   maxima-tui  ─────┼──▶ │  (the one process that does  │ ──▶  the game
   Maxima.app  ─────┤    │   everything: session, LSX,  │      (CrossOver
   maxima.exe  ─────┘    │   /authorize, RTM, installs) │       / Wine)
     (clients)           └──────────────────────────────┘
        via maxima-proto (typed JSON RPC over 127.0.0.1:13220)
```

Clients never talk to EA directly and never hold their own session — they connect to the server (spawning it on demand if it isn't running). The server owns a **status-bar icon** on every OS so you can open a UI or stop it without a terminal.

### Two runtime modes

| Mode | Where the binaries run | Status |
|------|------------------------|--------|
| **In-bottle** (what shipped releases use) | `maxima-*.exe` run **inside** the CrossOver bottle as Windows binaries; `MaximaHelper.app` bridges `qrc://` on the host. | Stable; what Draconis ships today. |
| **Native macOS** | `maxima-server` / `maxima-cli` / `maxima-bootstrap` run **natively on the host** (`aarch64-apple-darwin`) and drive per-game CrossOver bottles via `cxbottle`. | Validated end-to-end (login → install → TF2 to gameplay); not yet wired into Draconis. |

Both build from the same Rust workspace; the difference is only the target triple and where the process lives.

---

## Components — every part of the app

### Rust crates (the workspace)

| Crate | Binary | What it is |
|-------|--------|------------|
| **`maxima-lib`** | *(library)* | The core. EA auth/OAuth, license (OOA/`.dlf`), game library, LSX server, `/authorize` HTTP server, RTM presence, cloud saves, the content downloader, Steam-install discovery, and — on unix — Wine/CrossOver bottle management. **Everything else depends on it.** Also hosts [`server_client`](maxima-lib/src/server_client.rs) (see below). |
| **`maxima-proto`** | *(library)* | The **wire protocol** between the server and its clients: typed `Request` / `Response` / `Notification` envelopes + an async `MaximaClient` (connect, correlate replies, subscribe to broadcasts). Deliberately has **no `maxima-lib` dependency**, so a client can be truly thin. |
| **`maxima-server`** | `maxima-server` | **The one process that does everything.** Holds the session + LSX + `/authorize` + RTM + downloads, and serves many concurrent clients over `maxima-proto`. Owns the OS status-bar icon. Nothing talks to it except the CLI/TUI/GUI. |
| **`maxima-cli`** | `maxima-cli` | The command-line **client**. Product commands (`list-games`, `install`, `launch`, `verify`, `bottle-info`, `cloud-sync`, `register-protocols`, `locate-game`) forward to the server — the `--json` shapes Draconis consumes are unchanged. Also owns host operations that need no session: `service install/uninstall/status` (register the server with the OS), `server-stop/-status`, the interactive menu, low-level EA-API diagnostics, and self-contained manual/offline `launch --login`. |
| **`maxima-bootstrap`** | `maxima-bootstrap` | The **URL protocol handler** for `link2ea://` / `origin2://` / `qrc://`. Validates the offer id, probes `/authorize`, and forwards to a running server (else spawns `maxima-cli launch`). On macOS it's packaged as the bundle-signed `MaximaBootstrap.app` that LaunchServices registers. |
| **`maxima-tui`** | `maxima-tui` | Terminal UI. A **true thin client** — it renders the server's library over `maxima-proto`, no in-process session. |
| **`maxima-ui`** | `maxima` | The **egui** graphical UI (upstream's, patched to run under wgpu on Wine and natively on Metal). Still holds an in-process `maxima-lib` session; migrating it onto `MaximaClient` is the one remaining thin-client task (see [docs/CLEANUP.md](docs/CLEANUP.md) §6). |
| **`maxima-service`** | `maxima-service` | **Windows-only OS service** for KYBER DLL injection + boot-time registry setup. A no-op `main` elsewhere. **This is NOT the session server** — see the naming map below. Not exercised in the Draconis/Wine flow (Wine can't do `CreateRemoteThread` injection); shipped for upstream Windows parity. |
| **`maxima-resources`** | *(build helper)* | Build-time embedding of Windows `.exe` metadata + the `logo.ico` icon (via `winres`; no-op off Windows), plus the shared logo assets. A `[build-dependencies]` of every binary crate. |

### Host-side / native pieces (not Rust)

| Path | What it is |
|------|------------|
| **`MaximaHelper/`** | Native macOS Swift background agent. Bridges EA's `qrc://` OAuth redirect from the host browser into the bottle (`http://127.0.0.1:31033`). Bundle-signable so LaunchServices honors its scheme claim. Used by the **in-bottle** mode. |
| **`maxima-native/`** | The **native SwiftUI launcher** (`Maxima.app`, Liquid Glass, macOS 26+). A thin TCP client of `maxima-server` — library grid, install/launch progress, a friends rail, per-game settings, the boot-policy picker, and the menu-bar status icon. `build.sh` assembles the self-contained app; `make-pkg.sh` builds the installer `.pkg` + uninstaller. |
| **`installer/`** | The **NSIS Windows installer** (`maxima-setup.nsi`) + macOS cross-build script (`build.sh`, mingw-w64 + makensis). Drops the `.exe`s into the bottle and registers the Wine protocol handlers. `installer/autostart/` holds the launchd/systemd autostart templates. |
| **`docs/`** | [`CLEANUP.md`](docs/CLEANUP.md) (dead-code ledger), [`CONSUMER_MIGRATION.md`](docs/CONSUMER_MIGRATION.md) (how Draconis adopts the server-based Maxima), [`MACOS_BUNDLING.md`](docs/MACOS_BUNDLING.md) (install location + status-icon architecture). |
| **`.github/workflows/`** | `build-ci.yml` (3-OS push CI), `release.yml` (tag-driven release), `block-upstream-pr.yml` (guard against PR-ing fork changes upstream). |
| **`CLAUDE.md`** | The living engineering reference — architecture, gotchas, diagnostics, and a full changelog. Read this for depth. |

### "service" vs "server" — the naming map

The word **service** is overloaded across the tree (partly upstream, partly EA's own naming). They are unrelated:

| Name | What it actually is |
|------|---------------------|
| **`maxima-server`** (crate) | The multi-client **session server** — the process that does everything. |
| **`server_client.rs`** (in `maxima-lib`) | Client/host-side management of *that* server: discover it, spawn it on demand, and **register/unregister it with the OS** (`service install/uninstall`, boot policy). |
| **`maxima-service`** (crate) | The Windows **KYBER OS service** (DLL injection). Nothing to do with the session server. |
| **`util/service_win.rs`** (in `maxima-lib`) | Windows helpers that install/start *the `maxima-service` OS service*. |
| **`core/service_layer.rs`** (in `maxima-lib`) | Client for **EA's** "ServiceLayer" web API (players, catalog). EA's naming, not ours. |
| **`lsx/service.rs`** (in `maxima-lib`) | The **LSX** TCP listener (port 3216) the game authenticates against. |

---

## Boot policy — when the server starts

The server is independent of every GUI (closing a window never stops it). `maxima-cli service install --boot <policy>` (or the SwiftUI Settings picker) sets, per OS:

- **`auto`** — starts at login (macOS launchd LaunchAgent / Windows `HKCU\…\Run` / Linux systemd `--user` unit).
- **`on-demand`** *(default)* — no autostart; a frontend or a game launch spawns the detached server when it opens.
- **`manual`** — never auto-starts; you start it yourself.

`maxima-cli service uninstall [--purge]` removes the autostart, protocol claims, installed binaries, and config — **leaving no trace** that could interfere with the official EA app. macOS uses a classic LaunchAgent (not `SMAppService`, which needs a paid signing identity); binaries live at the stable `~/Library/Application Support/Maxima/bin`. See [docs/MACOS_BUNDLING.md](docs/MACOS_BUNDLING.md).

---

## Building from source

```bash
# Native macOS (server + clients + protocol handler)
cargo build --release -p maxima-server -p maxima-cli -p maxima-bootstrap
bash maxima-bootstrap/build-app.sh          # → MaximaBootstrap.app
bash maxima-native/build.sh                 # → maxima-native/build/Maxima.app
bash maxima-native/make-pkg.sh              # → Maxima-Installer.pkg (+ uninstall.sh)

# Windows in-bottle binaries (cross-compiled from macOS)
cargo +nightly build --release --target x86_64-pc-windows-gnu
bash installer/build.sh                     # → installer/MaximaSetup.exe (mingw-w64 + nsis)

# macOS qrc:// bridge for the in-bottle mode
bash MaximaHelper/build.sh

# Fast type-check during development
cargo check -p maxima-lib -p maxima-cli -p maxima-server -p maxima-proto
```

Requires Rust **nightly** (upstream uses `#![feature(...)]` gates). macOS-native builds target the host triple; the in-bottle build targets `x86_64-pc-windows-gnu`.

---

## Running it (native macOS)

```bash
maxima-cli list-games            # first run spawns the server, which does EA OAuth login
maxima-cli install titanfall-2   # creates a per-game CrossOver bottle, downloads into it
maxima-cli launch titanfall-2    # licenses + launches through CrossOver's wine
maxima-cli service status        # boot policy + whether the server is running
```

Or open `Maxima.app` — same server, graphical front. When a game emits `link2ea://`, `MaximaBootstrap.app` forwards it to the running server's `/authorize`, which relaunches the game with EA auth in place.

For the **in-bottle** flow, `MaximaSetup.exe` inside the bottle + `MaximaHelper.app` on the host; Draconis automates both. See [`CLAUDE.md`](./CLAUDE.md) for the full walkthrough.

---

## Diagnostics

```bash
# Is MaximaHelper registered for qrc:// on the host?
swift -e 'import AppKit; print(NSWorkspace.shared.urlForApplication(toOpen: URL(string:"qrc://x")!)?.path ?? "NONE")'

# Is the server up? (loopback ports — Wine forwards them to the host)
nc -zv 127.0.0.1 3216     # LSX
nc -zv 127.0.0.1 13219    # /authorize
maxima-cli server-status  # session state + client count
```

More recipes in [`CLAUDE.md`](./CLAUDE.md#diagnostics).

---

## Upstream & credits

This fork tracks [ArmchairDevelopers/Maxima](https://github.com/ArmchairDevelopers/Maxima). Draconis/macOS-specific work stays here; generic fixes go upstream when appropriate.

**Original Maxima creators:** [Sean Kahler](https://github.com/battledash) (creator), [Nick Whelan](https://github.com/headassbtw) (UI), [Paweł Lidwin](https://github.com/imLinguin) (core).
**Used by:** [AA-EION/Draconis](https://github.com/AA-EION/Draconis).
**Fork contributors:** [catornot](https://github.com/catornot) — `patch-external-lsx` (basis for the LSX-defensive handlers) + the `-noOriginStartup` requirement.

## License

GPL-3.0-or-later — same as upstream. See [LICENSE](./LICENSE).
