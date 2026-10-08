# Adopting the server-based Maxima (consumer migration guide)

Audience: anything that drives Maxima from the outside — **Draconis** is the
reference consumer, but Maxima is a *universal* EA launcher and this contract is
not Draconis- or Titanfall-specific. Per-title glue (Northstar, mod files, TF2
detection) stays in the consumer; Maxima only exposes machine-readable
primitives.

## TL;DR — what changed, what didn't

**Didn't change:** the `maxima-cli … --json` surface. Every command Draconis
already calls emits the **same JSONL, byte-for-byte**:

- `maxima-cli list-games --json`
- `maxima-cli install <slug> [--path …] [--build-id …] [--replace-files …] [--only-listed-files] --json`
- `maxima-cli launch <slug> [--game-args …] [-- …] --json` → `{"event":"launched"…}` / `{"event":"exited"…}` / `{"event":"error"…}`
- `maxima-cli verify <slug> --path … [--repair] --json`
- `maxima-cli bottle-info <slug> [--json]`
- `maxima-cli register-protocols`
- `maxima-cli cloud-sync <slug> [--write]`
- `maxima-cli download-specific-file …`

A consumer that only shells out to `maxima-cli --json` **needs no code changes**.
The CLI is now a thin client: it forwards each command to `maxima-server`, and
translates the server's proto notifications back into the exact JSONL shapes
above. Draconis's parsers are unaffected.

**Did change (under the hood):** those commands are now backed by a single
long-running **`maxima-server`** process. The first `maxima-cli` invocation
spawns it detached if it isn't already up; subsequent calls reuse it.

## Why a consumer should care anyway

Even though the CLI contract is stable, the persistent server changes the
runtime model in ways a good consumer accounts for:

1. **Login is now once-per-session, in the server.** Previously every
   `maxima-cli` call re-loaded auth and (on token expiry) could re-login. Now the
   server holds the session; the browser OAuth flow (`qrc://`) fires at most once,
   when the server first starts without a cached token. **Draconis's MaximaHelper
   still bridges `qrc://` → host loopback `:31033`**, so first-run login keeps
   working with no change. Nothing to do — just be aware the login prompt comes
   from the server process, not the per-command CLI.

2. **A background process outlives your command.** `maxima-cli list-games --json`
   returns, but `maxima-server` keeps running (holding LSX :3216, authorize
   :13219, RTM, and the control port :13220). This is intended — it's what lets a
   subsequent `launch` reuse the logged-in session and what serves the LSX auth
   when the game emits `link2ea://`. A consumer that wants to stop it calls
   `maxima-cli server-stop`; to check it, `maxima-cli server-status [--json]`.

3. **The server owns the status-bar icon** (see
   [MACOS_BUNDLING.md](MACOS_BUNDLING.md)). Draconis does **not** need to draw a
   "Maxima is running" indicator — the server surfaces one itself (menu-bar item
   on macOS, tray on Windows). If Draconis still wants its own indication, poll
   `server-status --json` (`{"running":true,"status":{persona,playing,installing,clients}}`).

## macOS native mode: what a consumer installs

The in-bottle mode (MaximaSetup.exe inside the CrossOver bottle + MaximaHelper.app
on the host) still ships and still works — **you do not have to migrate**. To use
the native host-side server, install these on the host at a **stable path**
(recommended `~/Library/Application Support/Maxima/bin/`, see
[MACOS_BUNDLING.md](MACOS_BUNDLING.md) for the rationale — app-translocation makes
`.app`-internal paths unstable):

- `maxima-server` — the process that does everything.
- `maxima-cli` — the client Draconis shells out to.
- `maxima-bootstrap` + `MaximaBootstrap.app` — the `link2ea://` / `origin2://`
  handler (bundle-signed; registered via `maxima-cli register-protocols`).

Optional but recommended: install the launchd agent
([installer/autostart/com.armchairdevelopers.maxima.server.plist](../installer/autostart/com.armchairdevelopers.maxima.server.plist))
so the server autostarts at login and the status icon is always present. Without
it, the first `maxima-cli` call spawns the server on demand — still correct, just
no icon until something invokes Maxima.

Draconis already ships MaximaHelper.app (for `qrc://`) and knows how to fetch
release assets; the incremental work is fetching + placing the three native
binaries and (optionally) loading the launchd agent. `maxima-cli` finds the
server via: bundled `Contents/Resources` → dev-tree `target/release` → **the
stable App Support path** → `PATH`.

## Launch flow, native mode (Draconis vanilla + Steam TF2)

Same end state as the in-bottle flow, host-side:

```
Draconis → maxima-cli launch <slug> --json
             → (spawns maxima-server if down) forwards Launch RPC
             → server: license preflight + EA env + cxstart-disclaimed spawn
             → {"event":"launched","offer_id":…,"wine_prefix":…}
   game emits link2ea:// → winebrowser → host `open` → MaximaBootstrap.app
             → probes :13219 → forwards to the *same* server's /authorize
   game exits → server detects (pgrep) → {"event":"exited","elapsed_secs":…}
```

Draconis never talks to `maxima-server` directly — only through `maxima-cli`
(the CLI is the client). That boundary is deliberate: the server's wire protocol
(`maxima-proto`) is an internal contract between Maxima's own frontends, not a
public API. Consumers target the stable `maxima-cli --json` surface.

## Checklist for Draconis

- [ ] No parser changes — the `--json` shapes are identical. Verify against the
      existing `list-games` / `install` / `launch` / `bottle-info` handling.
- [ ] (Native mode) Fetch + install `maxima-server`, `maxima-cli`,
      `maxima-bootstrap`, `MaximaBootstrap.app` into `~/Library/Application Support/Maxima/bin/`.
- [ ] (Native mode) Optionally install + `launchctl load` the launchd agent for
      autostart + persistent status icon.
- [ ] Keep MaximaHelper.app for `qrc://` (unchanged; coexists with MaximaBootstrap.app).
- [ ] Drop any assumption that a `maxima-cli` call is stateless/short-lived — a
      server persists after it. Use `server-status` to observe, `server-stop` to end.
- [ ] Don't draw your own Maxima tray/menu unless you want to — the server owns one.
