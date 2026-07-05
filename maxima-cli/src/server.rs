//! `maxima-cli server` — the multi-client Maxima server.
//!
//! One process holds the logged-in session, the LSX server, the `/authorize`
//! HTTP endpoint and the RTM connection, and serves **many** concurrent
//! clients over a loopback TCP socket (default `127.0.0.1:13220`, override
//! with `MAXIMA_SERVER_PORT`). Every client sees the same state: launch a
//! game from the CLI and the egui UI (also connected) shows it; a third-party
//! `link2ea://` that hits `/authorize` broadcasts `game-started` to all
//! clients. This is upstream PR #23's "Maxima Server" architecture (all logic
//! in one server, frontends as thin clients, states synced) in its pragmatic
//! fork-side form — the server→client notification layer that draft leaves
//! unfinished is implemented here as broadcast events.
//!
//! Wire protocol (newline-delimited JSON, one object per line):
//!   Client → server request:  {"id":N,"cmd":"list-games"|"friends"|"launch"|
//!                              "install"|"status"|"shutdown", …}
//!   Server → client response: {"id":N,"ok":true, …} | {"id":N,"ok":false,"error":"…"}
//!   Server → ALL clients event (no id): ready / presence / install-progress /
//!                              install-done / install-error / game-started /
//!                              game-stopped
//!
//! On connect the server sends a `ready` event (persona) to that client.
//! `shutdown` stops the whole server; `status` reports session state.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use log::{info, warn};
use maxima::core::{
    launch::{self, LaunchMode, LaunchOptions},
    LockedMaxima, MaximaEvent,
};
use maxima::rtm::client::RichPresence;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Mutex, Notify};

/// Default control port. LSX is 3216, authorize is 13219; the server control
/// channel is 13220.
pub const DEFAULT_PORT: u16 = 13220;

pub fn server_port() -> u16 {
    std::env::var("MAXIMA_SERVER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: u64,
    cmd: String,
    #[serde(default)]
    slug: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    exe_override: Option<String>,
    #[serde(default)]
    cloud_saves: Option<bool>,
}

struct ServerState {
    maxima: LockedMaxima,
    installing: Mutex<Option<String>>,
    events: broadcast::Sender<String>,
    shutdown: Notify,
    persona: Mutex<String>,
    clients: AtomicUsize,
}

impl ServerState {
    fn broadcast(&self, value: Value) {
        // Err just means no clients are currently subscribed — fine.
        let _ = self.events.send(value.to_string());
    }
}

pub async fn run_server(maxima_arc: LockedMaxima) -> Result<()> {
    let port = server_port();

    // Refuse to double-bind: if a server is already up, this process should
    // not have been started. Surface it clearly instead of a raw EADDRINUSE.
    let listener = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            anyhow::bail!(
                "a Maxima server is already running on 127.0.0.1:{} (stop it with \
                 `maxima-cli server-stop`)",
                port
            );
        }
        Err(err) => return Err(err.into()),
    };

    // --- Session setup: LSX + authorize + RTM, like `serve`/`ui-backend`. ---
    let persona = {
        let mut maxima = maxima_arc.lock().await;
        maxima.start_lsx(maxima_arc.clone()).await?;
        if let Err(err) = maxima.start_auth_server(maxima_arc.clone()).await {
            warn!("Authorize HTTP server failed to start: {}", err);
        }
        if let Err(err) = maxima.rtm().login().await {
            warn!("RTM login failed (continuing without presence): {}", err);
        } else {
            match maxima.friends(0).await {
                Ok(friends) => {
                    let players: Vec<String> =
                        friends.iter().map(|f| f.id().to_owned()).collect();
                    if let Err(err) = maxima.rtm().subscribe(&players).await {
                        warn!("Presence subscribe failed: {}", err);
                    } else {
                        info!("Subscribed to {} friends for presence", players.len());
                    }
                }
                Err(err) => warn!("Friends fetch failed: {}", err),
            }
        }
        let user = maxima.local_user().await?;
        user.player()
            .as_ref()
            .map(|p| p.display_name().to_string())
            .unwrap_or_default()
    };

    let (events_tx, _) = broadcast::channel::<String>(256);
    let state = Arc::new(ServerState {
        maxima: maxima_arc.clone(),
        installing: Mutex::new(None),
        events: events_tx,
        shutdown: Notify::new(),
        persona: Mutex::new(persona.clone()),
        clients: AtomicUsize::new(0),
    });

    info!(
        "Maxima server listening on 127.0.0.1:{} (persona: {})",
        port, persona
    );

    // Native tray on Windows — decoupled: it opens the UI / stops the server
    // by acting as an ordinary client on this port.
    #[cfg(windows)]
    crate::tray::spawn_tray(port);

    // Background tick task: drives maxima.update(), broadcasts presence /
    // install / game-lifecycle events to all clients. One per server.
    let tick_state = state.clone();
    tokio::spawn(async move { tick_loop(tick_state).await });

    // Accept loop, interruptible by shutdown.
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let s = state.clone();
                        tokio::spawn(async move { handle_client(s, stream).await; });
                    }
                    Err(err) => warn!("accept failed: {}", err),
                }
            }
            _ = state.shutdown.notified() => {
                info!("Shutdown requested — Maxima server stopping");
                break;
            }
        }
    }

    Ok(())
}

async fn tick_loop(state: Arc<ServerState>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut prev_presence: HashMap<String, RichPresence> = HashMap::new();
    let mut was_playing = false;
    let mut last_percent = -1.0_f64;

    loop {
        tick.tick().await;
        let mut maxima = state.maxima.lock().await;

        for event in maxima.consume_pending_events() {
            if let MaximaEvent::InstallFinished(offer_id) = event {
                let slug = state.installing.lock().await.take();
                state.broadcast(json!({
                    "event": "install-done", "offer_id": offer_id, "slug": slug
                }));
                last_percent = -1.0;
            }
        }

        maxima.update().await;

        let playing_now = maxima.playing().is_some();
        if was_playing && !playing_now {
            state.broadcast(json!({"event": "game-stopped"}));
        }
        was_playing = playing_now;

        let installing = state.installing.lock().await.clone();
        if let Some(slug) = installing {
            match maxima.content_manager().current() {
                Some(download) => {
                    let pct = download.percentage_done();
                    if (pct - last_percent).abs() > 0.05 {
                        state.broadcast(json!({
                            "event": "install-progress", "slug": slug, "percent": pct
                        }));
                        last_percent = pct;
                    }
                }
                None => {
                    *state.installing.lock().await = None;
                    state.broadcast(json!({"event": "install-done", "slug": slug}));
                    last_percent = -1.0;
                }
            }
        }

        let _ = maxima.rtm().heartbeat().await;
        {
            let store = maxima.rtm().presence_store().lock().await;
            for entry in store.iter() {
                let id: String = entry.0.as_ref().clone();
                let presence: RichPresence = entry.1;
                if prev_presence.get(&id) == Some(&presence) {
                    continue;
                }
                state.broadcast(json!({
                    "event": "presence",
                    "id": id,
                    "basic": format!("{:?}", presence.basic()),
                    "status": presence.status(),
                    "game": presence.game(),
                }));
                prev_presence.insert(id, presence);
            }
        }
    }
}

async fn handle_client(state: Arc<ServerState>, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let (read_half, mut write_half) = stream.into_split();
    let n = state.clients.fetch_add(1, Ordering::SeqCst) + 1;
    info!("client connected ({} total)", n);

    // Single writer task drains a per-client mpsc; both request responses and
    // forwarded broadcast events funnel through it so writes never interleave.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err()
                || write_half.write_all(b"\n").await.is_err()
            {
                break;
            }
            let _ = write_half.flush().await;
        }
    });

    // Forward broadcast events to this client.
    let mut events_rx = state.events.subscribe();
    let ev_tx = out_tx.clone();
    let forwarder = tokio::spawn(async move {
        loop {
            match events_rx.recv().await {
                Ok(line) => {
                    if ev_tx.send(line).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Greet with a ready snapshot.
    let persona = state.persona.lock().await.clone();
    let _ = out_tx.send(json!({"event": "ready", "persona": persona}).to_string());

    // Request loop.
    let reader = BufReader::new(read_half);
    let mut lines = reader.lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Request>(&line) {
                    Ok(req) => {
                        let id = req.id;
                        let is_shutdown = req.cmd == "shutdown";
                        let response = dispatch(&state, req).await;
                        let _ = out_tx.send(response.to_string());
                        if is_shutdown {
                            state.shutdown.notify_waiters();
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = out_tx.send(
                            json!({"id": id_hint(&line), "ok": false,
                                   "error": format!("bad request: {}", err)})
                            .to_string(),
                        );
                    }
                }
            }
            _ => break, // client disconnected
        }
    }

    forwarder.abort();
    drop(out_tx);
    let _ = writer.await;
    let remaining = state.clients.fetch_sub(1, Ordering::SeqCst) - 1;
    info!("client disconnected ({} remaining)", remaining);
}

fn id_hint(line: &str) -> u64 {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|v| v.get("id").and_then(|i| i.as_u64()))
        .unwrap_or(0)
}

/// Handle one request, returning the response object for the requesting
/// client. Events (broadcast to all) are emitted as a side effect.
async fn dispatch(state: &Arc<ServerState>, req: Request) -> Value {
    let id = req.id;
    let result: Result<Value> = match req.cmd.as_str() {
        "status" => Ok(status(state).await),
        "shutdown" => Ok(json!({"stopping": true})),
        "list-games" => {
            let mut maxima = state.maxima.lock().await;
            crate::games_json(&mut maxima)
                .await
                .map(|games| json!({"games": games}))
        }
        "friends" => {
            let maxima = state.maxima.lock().await;
            maxima.friends(0).await.map(|friends| {
                let list: Vec<Value> = friends
                    .iter()
                    .map(|f| json!({"id": f.id(), "name": f.display_name()}))
                    .collect();
                json!({"friends": list})
            }).map_err(Into::into)
        }
        "launch" => cmd_launch(state, req).await.map(|_| json!({})),
        "install" => cmd_install(state, req).await.map(|_| json!({})),
        other => Err(anyhow::anyhow!("unknown cmd `{}`", other)),
    };

    match result {
        Ok(mut extra) => {
            let obj = extra.as_object_mut().unwrap();
            obj.insert("id".into(), json!(id));
            obj.insert("ok".into(), json!(true));
            extra
        }
        Err(err) => json!({"id": id, "ok": false, "error": err.to_string()}),
    }
}

async fn status(state: &Arc<ServerState>) -> Value {
    let maxima = state.maxima.lock().await;
    json!({
        "status": {
            "persona": *state.persona.lock().await,
            "playing": maxima.playing().is_some(),
            "installing": *state.installing.lock().await,
            "lsx_port": maxima.lsx_port(),
            "clients": state.clients.load(Ordering::SeqCst),
        }
    })
}

/// Resolve a typed slug to (canonical_slug, offer_id); ensures the per-game
/// bottle on macOS so everything downstream targets the right prefix.
async fn resolve_game(maxima_arc: &LockedMaxima, typed: &str) -> Result<(String, String)> {
    let mut maxima = maxima_arc.lock().await;
    let slug = maxima.mut_library().canonical_slug(typed).await;
    let offer_id = maxima
        .mut_library()
        .game_by_base_slug(&slug)
        .await?
        .map(|o| o.offer_id().clone())
        .ok_or_else(|| anyhow::anyhow!("`{}` is not in this EA library", typed))?;
    drop(maxima);

    #[cfg(target_os = "macos")]
    maxima::unix::crossover::ensure_game_bottle(&slug).await?;

    Ok((slug, offer_id))
}

fn conventional_game_dir(slug: &str) -> Option<String> {
    let prefix = std::env::var("MAXIMA_WINE_PREFIX").ok()?;
    let dir = std::path::Path::new(&prefix)
        .join("drive_c")
        .join("Games")
        .join(slug);
    dir.exists().then(|| dir.to_string_lossy().to_string())
}

async fn cmd_launch(state: &Arc<ServerState>, req: Request) -> Result<()> {
    let typed = req.slug.ok_or_else(|| anyhow::anyhow!("launch requires `slug`"))?;
    let (slug, offer_id) = resolve_game(&state.maxima, &typed).await?;
    let path_override = req.exe_override.or_else(|| conventional_game_dir(&slug));

    launch::start_game(
        state.maxima.clone(),
        LaunchMode::Online(offer_id),
        LaunchOptions {
            path_override,
            arguments: req.args.unwrap_or_default(),
            cloud_saves: req.cloud_saves.unwrap_or(true),
            steam_app_id: None,
        },
    )
    .await?;

    state.broadcast(json!({"event": "game-started", "slug": slug}));
    Ok(())
}

async fn cmd_install(state: &Arc<ServerState>, req: Request) -> Result<()> {
    use maxima::content::manager::QueuedGameBuilder;

    if state.installing.lock().await.is_some() {
        anyhow::bail!("another install is already running");
    }
    let typed = req.slug.ok_or_else(|| anyhow::anyhow!("install requires `slug`"))?;
    let (slug, offer_id) = resolve_game(&state.maxima, &typed).await?;

    let install_path = match req.path {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let prefix = std::env::var("MAXIMA_WINE_PREFIX")
                .map_err(|_| anyhow::anyhow!("no path and no bottle selected for {}", slug))?;
            std::path::Path::new(&prefix)
                .join("drive_c")
                .join("Games")
                .join(&slug)
        }
    };

    let mut maxima = state.maxima.lock().await;
    let builds = maxima
        .content_manager()
        .service()
        .available_builds(&offer_id)
        .await?;
    let build = builds
        .live_build()
        .ok_or_else(|| anyhow::anyhow!("no live build for {}", slug))?;
    let game = QueuedGameBuilder::default()
        .offer_id(offer_id)
        .build_id(build.build_id().to_owned())
        .path(install_path)
        .build()?;
    maxima.content_manager().install_now(game).await?;
    drop(maxima);

    *state.installing.lock().await = Some(slug.clone());
    state.broadcast(json!({"event": "install-progress", "slug": slug, "percent": 0.0}));
    Ok(())
}

// ---------------------------------------------------------------------------
// Client side — used by the CLI (server-stop/status, launch/install forward)
// and by ensure_server_running() to auto-start the server for any frontend.
// ---------------------------------------------------------------------------

/// True if a server answers on the control port.
pub async fn is_running(port: u16) -> bool {
    tokio::time::timeout(
        std::time::Duration::from_millis(300),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

/// Connect and send one request, returning the matched response object.
async fn request_once(port: u16, req: Value) -> Result<Value> {
    let stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let (read_half, mut write_half) = stream.into_split();
    write_half.write_all(req.to_string().as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;

    let want_id = req.get("id").and_then(|i| i.as_u64()).unwrap_or(1);
    let reader = BufReader::new(read_half);
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("id").and_then(|i| i.as_u64()) == Some(want_id) {
            return Ok(v);
        }
    }
    anyhow::bail!("server closed the connection before responding")
}

pub async fn send_shutdown(port: u16) -> Result<()> {
    if !is_running(port).await {
        println!("No Maxima server running on port {}.", port);
        return Ok(());
    }
    let resp = request_once(port, json!({"id": 1, "cmd": "shutdown"})).await?;
    if resp.get("ok").and_then(|b| b.as_bool()) == Some(true) {
        println!("Maxima server on port {} is stopping.", port);
    }
    Ok(())
}

pub async fn print_status(port: u16, json_out: bool) -> Result<()> {
    if !is_running(port).await {
        if json_out {
            println!("{}", json!({"running": false, "port": port}));
        } else {
            println!("Maxima server: not running (port {}).", port);
        }
        return Ok(());
    }
    let resp = request_once(port, json!({"id": 1, "cmd": "status"})).await?;
    let status = resp.get("status").cloned().unwrap_or(json!({}));
    if json_out {
        println!("{}", json!({"running": true, "port": port, "status": status}));
    } else {
        println!("Maxima server: running on port {}", port);
        if let Some(p) = status.get("persona").and_then(|v| v.as_str()) {
            println!("  persona:    {}", p);
        }
        println!(
            "  playing:    {}",
            status.get("playing").and_then(|v| v.as_bool()).unwrap_or(false)
        );
        if let Some(slug) = status.get("installing").and_then(|v| v.as_str()) {
            println!("  installing: {}", slug);
        }
        println!(
            "  clients:    {}",
            status.get("clients").and_then(|v| v.as_u64()).unwrap_or(0)
        );
    }
    Ok(())
}

/// Ensure a server is up, spawning `maxima-cli server` detached if not, and
/// waiting until its control port answers. Used by frontends (and forwarding
/// CLI commands) so the server auto-starts when it isn't already running at
/// logon.
pub async fn ensure_server_running(port: u16) -> Result<()> {
    if is_running(port).await {
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    info!("No server on port {}; starting one ({})", port, exe.display());

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("server")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Detach from our process group so the server outlives the spawning
    // frontend / CLI command.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    cmd.spawn()?;

    // Wait for it to come up (login may run; give it room).
    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if is_running(port).await {
            return Ok(());
        }
    }
    anyhow::bail!("server did not come up within 60s")
}

/// Forward a launch/install to the running server and stream its events to
/// stdout until a terminal event for this action arrives. `terminal` names
/// the event(s) that end the stream. In `json_out` mode raw event lines are
/// passed through; otherwise they're logged human-readably.
pub async fn forward_streaming(
    port: u16,
    request: Value,
    terminal: &[&str],
    json_out: bool,
) -> Result<()> {
    let stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let (read_half, mut write_half) = stream.into_split();
    let want_id = request.get("id").and_then(|i| i.as_u64()).unwrap_or(1);
    write_half.write_all(request.to_string().as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;

    let reader = BufReader::new(read_half);
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Error on our request → surface and stop.
        if v.get("id").and_then(|i| i.as_u64()) == Some(want_id)
            && v.get("ok").and_then(|b| b.as_bool()) == Some(false)
        {
            anyhow::bail!(
                "{}",
                v.get("error").and_then(|e| e.as_str()).unwrap_or("request failed")
            );
        }
        if let Some(ev) = v.get("event").and_then(|e| e.as_str()) {
            if json_out {
                println!("{}", line);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            } else {
                match ev {
                    "install-progress" => {
                        if let Some(p) = v.get("percent").and_then(|p| p.as_f64()) {
                            info!("Downloading: {:.1}%/100%", p);
                        }
                    }
                    "install-done" => info!("Install complete."),
                    "install-error" => warn!(
                        "Install error: {}",
                        v.get("message").and_then(|m| m.as_str()).unwrap_or("?")
                    ),
                    "game-started" => info!("Game started."),
                    "game-stopped" => info!("Game stopped."),
                    _ => {}
                }
            }
            if terminal.contains(&ev) {
                break;
            }
        }
    }
    Ok(())
}
