<p align="center">
  <img src="images/logo.png" width="120" alt="Maxima logo" />
</p>

<h1 align="center">Maxima</h1>

<p align="center">
  A free, open-source replacement for the EA Desktop Launcher.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-informational" alt="Platforms" />
  <img src="https://img.shields.io/badge/Rust-nightly-F74C00?logo=rust&logoColor=white" alt="Rust nightly" />
  <img src="https://img.shields.io/github/license/ArmchairDevelopers/Maxima?color=blue" alt="GPL-3.0" />
</p>

---

> [!WARNING]
> Maxima is pre-pre-pre-alpha software, released early to support [KYBER](https://github.com/ArmchairDevelopers/Kyber). Standalone use is unsupported upstream — bug fixes and [contributions](CONTRIBUTING.md) are welcome, but expect rough edges.

**This is the Maxima-Draconis fork.** It extends the upstream project with macOS/CrossOver compatibility patches, a native Swift protocol helper, and a Windows installer — intended as the EA authentication and launch backend for [Draconis](https://github.com/AA-EION/Draconis). For the canonical project, see [ArmchairDevelopers/Maxima](https://github.com/ArmchairDevelopers/Maxima).

---

## Features

- **EA Authentication** — OAuth login flow with a `remid`-cookie fallback for macOS browsers that cannot handle `qrc://` redirects
- **Download & Update games** — any build, with DRM and licensing support
- **Launch EA games from Steam or Epic** — Steam App IDs resolve automatically to EA offer IDs
- **`link2ea://` and `origin2://` protocol handlers** — so Steam/Epic can invoke Maxima directly
- **Offline mode** — launch single-player titles using a cached local license
- **EA cloud save sync**
- **Friends / social status**
- **Game importing** (locate existing installations)
- **Linux/SteamDeck** — runs under [proton-ge](https://github.com/GloriousEggroll/proton-ge-custom) via [umu](https://github.com/Open-Wine-Components/umu-launcher), installed automatically
- **macOS/CrossOver** — install `MaximaSetup.exe` inside a CrossOver bottle; see [Setup](#macos--crossover-setup)

**In development:**
- Full macOS support (CrossOver and Apple Game Porting Toolkit)

**Planned:**
- DLC installs
- Progressive / selective installs
- Store integration (buying games)
- Full EA Desktop interoperability
- Friend management and status control
- Multi-frontend architecture

**Not yet supported:**
- Battlefield 3 / 4 (Battlelog launch flow)
- Pre-Download-In-Place era games (Dead Space 2, BFBC2)

---

## Project layout

```
maxima-lib/         Core library (auth, launch, library, cloud save, friends)
maxima-cli/         Interactive CLI and subcommand frontend
maxima-tui/         Terminal UI frontend
maxima-ui/          GUI frontend
maxima-bootstrap/   Windows bootstrap process — handles link2ea:// / origin2:// URIs
maxima-service/     Background Windows service
maxima-resources/   Shared assets (icons, etc.)
MaximaHelper/       Native macOS Swift app — registers qrc:// on the host Mac
installer/          NSIS installer script + cross-build script (macOS → Windows)
```

---

## macOS / CrossOver setup

Maxima runs **inside** a CrossOver or Wine bottle. The Mac itself needs the `MaximaHelper` background agent so that EA's `qrc://` login redirect is caught natively rather than inside Wine.

**One-time host setup (run outside CrossOver):**

```bash
# Build and register MaximaHelper.app (~5 seconds, requires Xcode CLT)
bash MaximaHelper/build.sh
```

**Install Maxima inside your CrossOver bottle:**

```bash
# Cross-compile from macOS (requires mingw-w64 and nsis: brew install mingw-w64 nsis)
bash installer/build.sh
# → produces installer/MaximaSetup.exe
```

Then run `MaximaSetup.exe` inside your CrossOver bottle. It registers the `link2ea://`, `origin2://`, and `qrc://` protocol handlers, installs the background service, and creates start menu shortcuts.

---

## Building from source

Requires Rust nightly and the workspace dependencies.

```bash
# Native build (current platform)
cargo build --release

# Cross-compile for Windows from macOS
bash installer/build.sh
```

See [`changes.md`](./changes.md) for a full list of patches applied on top of upstream, and [`todo.md`](./todo.md) for the remaining work.

---

## CLI usage

```bash
# Interactive mode
maxima-cli

# Subcommand help
maxima-cli help
# locate-game, cloud-sync, create-auth-code, list-friends, launch, ...
```

---

## Why "Maxima"?

It's the farthest you can get from the Origin.

---

## Credits

Maxima was created and is maintained by [ArmchairDevelopers](https://github.com/ArmchairDevelopers). This fork exists solely to support [Draconis](https://github.com/AA-EION/Draconis) and tracks upstream closely.

**Original creators:**

- [Sean Kahler](https://github.com/battledash) — creator of Maxima
- [Nick Whelan](https://github.com/headassbtw) — UI maintainer
- [Paweł Lidwin](https://github.com/imLinguin) — core maintainer

**Upstream project:** [ArmchairDevelopers/Maxima](https://github.com/ArmchairDevelopers/Maxima)  
**Sister project:** [KYBER](https://uplink.kyber.gg/news/features-overview)

---

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md). For issues specific to macOS/CrossOver, open them here. For core Maxima issues, consider contributing upstream.

---

## License

GPL-3.0-or-later — same as upstream. See [LICENSE](./LICENSE).
