# Leftovers & dead code — cleanup ledger

State of the world as of the server-does-everything refactor (server split into
`maxima-server`, CLI became a pure client). This file tracks what is now
**unreachable**, what is **redundant**, and what is **half-done**, plus the plan
to remove each. Keep it honest: when an item is cleaned, delete its row (git
remembers). When you notice new leftovers, add a row.

> Rule of thumb: the CLI, TUI and GUI are **clients**. `maxima-server` is the
> only thing that holds a `maxima-lib` session. Any code in a client that logs
> in / starts LSX / downloads / launches in-process is a leftover unless it is
> one of the two deliberate exceptions (see "Kept on purpose").

---

> **Status:** §1 and §2 below are **DONE** (commit `refactor(cli): delete dead
> in-process product commands; forward locate-game`) — kept here as a record of
> what was removed. §5 (status icon) is **DONE** (server owns the icon on all
> OSes). §3, §4, §6 remain.

## 1. `maxima-cli` — dormant in-process implementations (DEAD) — ✅ removed

`startup()` in [maxima-cli/src/main.rs](../maxima-cli/src/main.rs) has a
*forward-first* block near the top (~L596–652). It routes every product command
to the server via `server::run_*` (each `ensure_server_running`s, so the server
is spawned if down) and **returns before** the legacy in-process match arms are
reached. That makes the following unreachable:

| Dead fn (main.rs) | Reached by | Why dead |
|---|---|---|
| `download_specific_file` (~L1140) | `DownloadSpecificFile` | forwarded → `server::run_download_file` |
| `games_json` (~L1433) + `GameJson`/`ExtraOfferJson` structs | only `list_games` | its only caller is dead |
| `list_games` (~L1507) | `ListGames` | forwarded → `server::run_list_games` |
| `install_game` (~L1567, ~380 lines) | `Install` | forwarded → `server::run_install` |
| `verify_game` (~L1946, ~270 lines) | `Verify` | forwarded → `server::run_verify` |
| `bottle_info` (~L2213) | `BottleInfo` | forwarded → `server::run_bottle_info` |
| `do_cloud_sync` (~L2310) | `CloudSync` | forwarded → `server::run_cloud_sync` |
| `canonical_slug` (~L1562, macOS) | install/launch in-proc arms | callers below are dead |
| `Mode::Launch { login: None }` arm (~L706–881) | `launch` w/o `--login` | forwarded → `server::forward_streaming` |
| in-proc match arms in `startup()` for `ListGames`, `Install`, `Verify`, `CloudSync`, `DownloadSpecificFile`, `BottleInfo`, and the `RegisterProtocols` unix arm | — | superseded by the forward-first block |

**Plan.** Delete the dead fns and their now-unreachable `startup()` match arms.
The `Mode::Launch` arm keeps only the `login: Some(_)` (manual/offline) branch —
strip the `login: None` resolution + Steam-path + bottle logic from it. Remove
`GameJson`/`ExtraOfferJson`. Prune imports that only those fns used
(`ContentService`, `QueuedGameBuilder`, `ZipDownloader`, `manifest`,
`CloudSyncLockMode`, the steam helpers if unused elsewhere). Build for
`aarch64-apple-darwin` **and** `x86_64-pc-windows-gnu` after — the arms are
`#[cfg]`-heavy, so a one-target build can hide a break.

Net: roughly **−1000 lines** from `main.rs`, leaving it a thin client +
interactive/diagnostic shell.

---

## 2. `maxima-cli` — inconsistent forwarding — ✅ fixed

- **`Mode::LocateGame`** now forwards via `server::run_locate_game`
  (`client.locate_game`), and the in-process `locate_game` was deleted.

## 3. `maxima-cli` — entry points that duplicate the server (REDUNDANT, decide)

Not dead (still reachable), but they re-implement what the server now owns:

- **`Mode::Serve`** (`serve_lsx`, ~L2502) — starts LSX + authorize + RTM and
  parks. That is *exactly* what `maxima-server` does. `serve` predates the
  server. **Recommendation:** keep as a thin alias that just spawns/leaves the
  server running (or deprecate with a message pointing at the server), rather
  than a second session implementation. Low priority — it works and is small.
- **Interactive menu** (`run_interactive` + `interactive_*`, `generate_download_links`)
  — the "no subcommand" TUI-ish flow. Duplicates the TUI/GUI. **Recommendation:**
  leave for now (handy for quick manual login/testing), but it should eventually
  become a `MaximaClient` flow like the TUI. Not urgent.

## 4. `maxima-cli` — developer/diagnostic subcommands (KEEP)

`AccountInfo`, `CreateAuthCode`, `JunoTokenRefresh`, `ReadLicenseFile`,
`ListFriends`, `GetUserById`, `GetGameBySlug`, `TestRTMConnection`,
`GetLegacyCatalogDef` stay in-process on purpose — they're low-level EA-API
probes for debugging, not product commands, and forwarding them through the
proto would mean adding RPCs nobody but a developer calls. Left as-is.

## Kept on purpose (NOT leftovers)

- `Mode::Launch { login: Some(_) }` — manual/offline login is self-contained
  (only the license server needs auth); it deliberately never touches the server.
- `Mode::ServerStop` / `Mode::ServerStatus` — pure client control commands.
- The diagnostics in §4.

---

## 5. Status icon — ✅ server-owned on all OSes

`maxima-server/src/status_icon.rs` dispatches per-OS: Windows tray (now the
`logo.ico`), macOS spawns the `Maxima.app --menubar` host, Linux SNI tray behind
the off-by-default `linux-tray` feature. See [MACOS_BUNDLING.md](MACOS_BUNDLING.md).

## 6. egui `maxima-ui` — still an in-process session (HALF-DONE, task #24)

The egui UI is the last frontend that still holds its own `Maxima` and only
defers LSX to the server. The server side is ready (proto has `who-am-i`,
`game-images`, `game-details`, `friends` + avatars, launch/install/verify/…).
Remaining is purely inside `maxima-ui`: rewire `bridge_thread` / `event_thread`
/ `bridge/*` onto a `MaximaClient`, and change two response types
(`InteractThreadLoginResponse.you: ServicePlayer` → persona/id strings;
`GameInfo.dlc: Vec<OwnedOffer>` → `Vec<ExtraOfferDto>`). Tracked separately;
it's ~600 lines across a working 5.5k-line UI, so it's staged, not rushed.

Once §6 lands, `maxima-lib` is depended on by *only* `maxima-server` (+ the
build-time host utilities the clients use: logging, registry check). That's the
PR #23 end state.
