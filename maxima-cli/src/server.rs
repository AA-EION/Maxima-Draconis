//! `maxima-cli server` — the multi-client Maxima server.
//!
//! One process holds the logged-in session, the LSX server, the `/authorize`
//! HTTP endpoint and the RTM connection, and serves **many** concurrent
//! clients over a loopback TCP socket (default `127.0.0.1:13220`, override
//! with `MAXIMA_SERVER_PORT`). Every client sees the same state.
//!
//! This is upstream PR #23's "Maxima Server": one server, frontends as thin
//! clients. The wire protocol and the client live in the `maxima-proto`
//! crate (typed [`maxima_proto::Request`] / [`ResponseEnvelope`] /
//! [`Notification`]); this module is the server side — it dispatches those
//! requests against the real `maxima-lib` `Maxima` and broadcasts
//! notifications to every client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use log::{info, warn};
use maxima::core::{
    cloudsync::CloudSyncLockMode,
    launch::{self, LaunchMode, LaunchOptions},
    manifest, LockedMaxima, MaximaEvent,
};
use maxima::rtm::client::RichPresence;
use maxima_proto::message::{Notification, Request, RequestEnvelope, ResponseEnvelope};
use maxima_proto::types::{FriendDto, GameDetailsDto, StatusDto};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Mutex, Notify};

/// Default control port. LSX is 3216, authorize is 13219; the server control
/// channel is 13220.
pub const DEFAULT_PORT: u16 = maxima_proto::DEFAULT_PORT;

pub fn server_port() -> u16 {
    maxima_proto::server_port()
}

struct ServerState {
    maxima: LockedMaxima,
    installing: Mutex<Option<String>>,
    /// Serialized [`Notification`] lines, broadcast to every client.
    events: broadcast::Sender<String>,
    shutdown: Notify,
    persona: Mutex<String>,
    clients: AtomicUsize,
}

impl ServerState {
    fn notify(&self, note: Notification) {
        // Err just means no clients are currently subscribed — fine.
        if let Ok(line) = serde_json::to_string(&note) {
            let _ = self.events.send(line);
        }
    }
}

pub async fn run_server(maxima_arc: LockedMaxima) -> Result<()> {
    let port = server_port();

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

    // --- Session setup: LSX + authorize + RTM, like `serve`. ---
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

    info!("Maxima server listening on 127.0.0.1:{} (persona: {})", port, persona);

    #[cfg(windows)]
    crate::tray::spawn_tray(port);

    let tick_state = state.clone();
    tokio::spawn(async move { tick_loop(tick_state).await });

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
            if let MaximaEvent::InstallFinished(_offer_id) = event {
                let slug = state.installing.lock().await.take();
                state.notify(Notification::InstallDone { slug });
                state.notify(Notification::DownloadQueue { current: None, queued: vec![] });
                last_percent = -1.0;
            }
        }

        maxima.update().await;

        let playing_now = maxima.playing().is_some();
        if was_playing && !playing_now {
            state.notify(Notification::GameStopped);
        }
        was_playing = playing_now;

        let installing = state.installing.lock().await.clone();
        if let Some(slug) = installing {
            match maxima.content_manager().current() {
                Some(download) => {
                    let pct = download.percentage_done();
                    if (pct - last_percent).abs() > 0.05 {
                        state.notify(Notification::InstallProgress {
                            slug: slug.clone(),
                            percent: pct,
                        });
                        last_percent = pct;
                    }
                }
                None => {
                    *state.installing.lock().await = None;
                    state.notify(Notification::InstallDone { slug: Some(slug) });
                    state.notify(Notification::DownloadQueue { current: None, queued: vec![] });
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
                state.notify(Notification::Presence {
                    id: id.clone(),
                    basic: format!("{:?}", presence.basic()),
                    status: presence.status().clone(),
                    game: presence.game().clone(),
                });
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
    if let Ok(line) = serde_json::to_string(&Notification::Ready { persona }) {
        let _ = out_tx.send(line);
    }

    let reader = BufReader::new(read_half);
    let mut lines = reader.lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<RequestEnvelope>(&line) {
                    Ok(env) => {
                        let id = env.id;
                        let is_shutdown = matches!(env.request, Request::Shutdown);
                        let response = dispatch(&state, id, env.request).await;
                        if let Ok(line) = serde_json::to_string(&response) {
                            let _ = out_tx.send(line);
                        }
                        if is_shutdown {
                            state.shutdown.notify_waiters();
                            break;
                        }
                    }
                    Err(err) => {
                        let resp = ResponseEnvelope::err(
                            id_hint(&line),
                            format!("bad request: {}", err),
                        );
                        if let Ok(line) = serde_json::to_string(&resp) {
                            let _ = out_tx.send(line);
                        }
                    }
                }
            }
            _ => break,
        }
    }

    forwarder.abort();
    drop(out_tx);
    let _ = writer.await;
    let remaining = state.clients.fetch_sub(1, Ordering::SeqCst) - 1;
    info!("client disconnected ({} remaining)", remaining);
}

fn id_hint(line: &str) -> u64 {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("id").and_then(|i| i.as_u64()))
        .unwrap_or(0)
}

/// Handle one request, returning the response for the requesting client.
/// Notifications (broadcast to all) are emitted as a side effect.
async fn dispatch(state: &Arc<ServerState>, id: u64, request: Request) -> ResponseEnvelope {
    let result: Result<serde_json::Value> = match request {
        Request::Status => Ok(json!({ "status": status(state).await })),
        Request::Shutdown => Ok(json!({ "stopping": true })),
        Request::ListGames => {
            let mut maxima = state.maxima.lock().await;
            crate::games_json(&mut maxima).await.map(|games| json!({ "games": games }))
        }
        Request::Friends => {
            let maxima = state.maxima.lock().await;
            maxima
                .friends(0)
                .await
                .map(|friends| {
                    let list: Vec<FriendDto> = friends
                        .iter()
                        .map(|f| FriendDto {
                            id: f.id().clone(),
                            name: f.display_name().to_string(),
                            avatar_url: f
                                .avatar()
                                .as_ref()
                                .map(|a| a.medium().path().to_string()),
                        })
                        .collect();
                    json!({ "friends": list })
                })
                .map_err(Into::into)
        }
        Request::WhoAmI => whoami(state).await.map(|u| json!({ "user": u })),
        Request::GameDetails { slug } => {
            game_details(state, &slug).await.map(|d| json!({ "details": d }))
        }
        Request::GameImages { slug } => {
            game_images(state, &slug).await.map(|i| json!({ "images": i }))
        }
        Request::Launch { slug, args, exe_override, cloud_saves } => cmd_launch(
            state, slug, args, exe_override, cloud_saves,
        )
        .await
        .map(|_| json!({})),
        Request::Install { slug, path } => {
            cmd_install(state, slug, path).await.map(|_| json!({}))
        }
        Request::LocateGame { path } => cmd_locate(state, &path).await.map(|_| json!({})),
        Request::CloudSync { slug, write } => {
            cmd_cloud_sync(state, &slug, write).await.map(|_| json!({}))
        }
    };

    match result {
        Ok(data) => ResponseEnvelope::ok(id, data),
        Err(err) => ResponseEnvelope::err(id, err.to_string()),
    }
}

async fn status(state: &Arc<ServerState>) -> StatusDto {
    let maxima = state.maxima.lock().await;
    StatusDto {
        persona: state.persona.lock().await.clone(),
        playing: maxima.playing().is_some(),
        installing: state.installing.lock().await.clone(),
        lsx_port: *maxima.lsx_port(),
        clients: state.clients.load(Ordering::SeqCst) as u64,
    }
}

async fn whoami(state: &Arc<ServerState>) -> Result<maxima_proto::types::UserDto> {
    let maxima = state.maxima.lock().await;
    let user = maxima.local_user().await?;
    let player = user
        .player()
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no local player"))?;
    Ok(maxima_proto::types::UserDto {
        id: user.id().to_string(),
        name: player.display_name().to_string(),
        avatar_url: player.avatar().as_ref().map(|a| a.medium().path().to_string()),
    })
}

/// Fetch a game's hero / logo / background image URLs from the service layer.
/// Mirrors the image selection the egui UI's `get_games::handle_images` did
/// in-process; the UI now just downloads whichever URLs come back.
async fn game_images(state: &Arc<ServerState>, slug: &str) -> Result<maxima_proto::types::GameImagesDto> {
    use maxima::core::service_layer::{
        ServiceGame, ServiceGameHubCollection, ServiceGameImagesRequestBuilder,
        ServiceHeroBackgroundImageRequestBuilder, SERVICE_REQUEST_GAMEIMAGES,
        SERVICE_REQUEST_GETHEROBACKGROUNDIMAGE,
    };

    // Same selection order get_games::handle_images used (the getters return
    // `&Option<..>`, hence the explicit if-let chains rather than combinators).
    fn pick_hero(images: &Option<ServiceGame>) -> Option<String> {
        let key_art = match images {
            Some(i) => i.key_art(),
            None => return None,
        };
        let key_art = match key_art {
            Some(k) => k,
            None => return None,
        };
        if let Some(img) = key_art.aspect_10x3_image() {
            return Some(img.path().clone());
        }
        if let Some(img) = key_art.aspect_2x1_image() {
            return Some(img.path().clone());
        }
        if let Some(img) = key_art.aspect_16x9_image() {
            return Some(img.path().clone());
        }
        None
    }
    fn pick_logo(images: &Option<ServiceGame>) -> Option<String> {
        let logo_set = match images {
            Some(i) => i.primary_logo(),
            None => return None,
        };
        match logo_set {
            Some(logo) => logo.largest_image().as_ref().map(|l| l.path().clone()),
            None => None,
        }
    }
    fn pick_bg(heroes: &Option<ServiceGameHubCollection>) -> Option<String> {
        let hero = match heroes {
            Some(h) => h.items().get(0),
            None => return None,
        };
        let bg = match hero {
            Some(bg) => bg.hero_background(),
            None => return None,
        };
        if let Some(img) = bg.aspect_16x9_image() {
            return Some(img.path().clone());
        }
        if let Some(img) = bg.aspect_2x1_image() {
            return Some(img.path().clone());
        }
        if let Some(img) = bg.aspect_10x3_image() {
            return Some(img.path().clone());
        }
        None
    }

    let (service_layer, locale) = {
        let maxima = state.maxima.lock().await;
        (maxima.service_layer().clone(), maxima.locale().short_str().to_owned())
    };

    let images: Option<ServiceGame> = service_layer
        .request(
            SERVICE_REQUEST_GAMEIMAGES,
            ServiceGameImagesRequestBuilder::default()
                .should_fetch_context_image(true)
                .should_fetch_backdrop_images(true)
                .game_slug(slug.to_owned())
                .locale(locale.clone())
                .build()?,
        )
        .await
        .ok()
        .flatten();

    let heroes: Option<ServiceGameHubCollection> = service_layer
        .request(
            SERVICE_REQUEST_GETHEROBACKGROUNDIMAGE,
            ServiceHeroBackgroundImageRequestBuilder::default()
                .game_slug(slug.to_owned())
                .locale(locale)
                .build()?,
        )
        .await
        .ok()
        .flatten();

    Ok(maxima_proto::types::GameImagesDto {
        hero: pick_hero(&images),
        logo: pick_logo(&images),
        background: pick_bg(&heroes),
    })
}

async fn game_details(state: &Arc<ServerState>, slug: &str) -> Result<GameDetailsDto> {
    use maxima::core::service_layer::{
        ServiceGameSystemRequirements, ServiceGameSystemRequirementsRequestBuilder,
        SERVICE_REQUEST_GAMESYSTEMREQUIREMENTS,
    };

    let maxima = state.maxima.lock().await;
    let rq: ServiceGameSystemRequirements = maxima
        .service_layer()
        .request(
            SERVICE_REQUEST_GAMESYSTEMREQUIREMENTS,
            ServiceGameSystemRequirementsRequestBuilder::default()
                .slug(slug.to_owned())
                .locale(maxima.locale().short_str().to_owned())
                .build()?,
        )
        .await?;

    // Return raw HTML system-requirement blocks; the egui UI applies its
    // easymark transform when mapping the DTO onto its own type.
    let (min, rec) = if !rq.system_requirements().is_empty() {
        (
            Some(rq.system_requirements()[0].minimum().to_owned()),
            Some(rq.system_requirements()[0].recommended().to_owned()),
        )
    } else {
        (None, None)
    };

    Ok(GameDetailsDto {
        time: 0,
        achievements_unlocked: 0,
        achievements_total: 0,
        path: String::new(),
        system_requirements_min: min,
        system_requirements_rec: rec,
    })
}

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
    let dir = std::path::Path::new(&prefix).join("drive_c").join("Games").join(slug);
    dir.exists().then(|| dir.to_string_lossy().to_string())
}

async fn cmd_launch(
    state: &Arc<ServerState>,
    typed: String,
    args: Vec<String>,
    exe_override: Option<String>,
    cloud_saves: bool,
) -> Result<()> {
    let (slug, offer_id) = resolve_game(&state.maxima, &typed).await?;
    let path_override = exe_override.or_else(|| conventional_game_dir(&slug));

    launch::start_game(
        state.maxima.clone(),
        LaunchMode::Online(offer_id),
        LaunchOptions { path_override, arguments: args, cloud_saves, steam_app_id: None },
    )
    .await?;

    state.notify(Notification::GameStarted { slug });
    Ok(())
}

async fn cmd_install(state: &Arc<ServerState>, typed: String, path: Option<String>) -> Result<()> {
    use maxima::content::manager::QueuedGameBuilder;

    if state.installing.lock().await.is_some() {
        anyhow::bail!("another install is already running");
    }
    let (slug, offer_id) = resolve_game(&state.maxima, &typed).await?;

    let install_path = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let prefix = std::env::var("MAXIMA_WINE_PREFIX")
                .map_err(|_| anyhow::anyhow!("no path and no bottle selected for {}", slug))?;
            std::path::Path::new(&prefix).join("drive_c").join("Games").join(&slug)
        }
    };

    let mut maxima = state.maxima.lock().await;
    let builds = maxima.content_manager().service().available_builds(&offer_id).await?;
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
    state.notify(Notification::InstallProgress { slug: slug.clone(), percent: 0.0 });
    state.notify(Notification::DownloadQueue { current: Some(slug), queued: vec![] });
    Ok(())
}

async fn cmd_locate(state: &Arc<ServerState>, path: &str) -> Result<()> {
    let path = std::path::PathBuf::from(path);
    let man = manifest::read(path.join(maxima::core::manifest::MANIFEST_RELATIVE_PATH)).await?;
    man.run_touchup(&path).await?;
    // Refresh library so the located game shows as installed to every client.
    let _ = state.maxima.lock().await.mut_library().games().await;
    Ok(())
}

async fn cmd_cloud_sync(state: &Arc<ServerState>, slug: &str, write: bool) -> Result<()> {
    let mut maxima = state.maxima.lock().await;
    let offer = maxima
        .mut_library()
        .game_by_base_slug(slug)
        .await?
        .ok_or_else(|| anyhow::anyhow!("`{}` not in library", slug))?
        .clone();
    let mode = if write { CloudSyncLockMode::Write } else { CloudSyncLockMode::Read };
    let lock = maxima.cloud_sync().obtain_lock(&offer, mode).await?;
    let res = lock.sync_files().await;
    lock.release().await?;
    res?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Client side — the CLI's own use of the server (server-stop / server-status,
// launch/install forwarding). Built on maxima_proto::MaximaClient.
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

pub async fn send_shutdown(port: u16) -> Result<()> {
    if !is_running(port).await {
        println!("No Maxima server running on port {}.", port);
        return Ok(());
    }
    let client = maxima_proto::MaximaClient::connect(port).await?;
    client.shutdown().await?;
    println!("Maxima server on port {} is stopping.", port);
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
    let client = maxima_proto::MaximaClient::connect(port).await?;
    let status = client.status().await?;
    if json_out {
        println!(
            "{}",
            json!({"running": true, "port": port, "status": status})
        );
    } else {
        println!("Maxima server: running on port {}", port);
        println!("  persona:    {}", status.persona);
        println!("  playing:    {}", status.playing);
        if let Some(slug) = &status.installing {
            println!("  installing: {}", slug);
        }
        println!("  clients:    {}", status.clients);
    }
    Ok(())
}

/// Ensure a server is up, spawning `maxima-cli server` detached if not.
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
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    cmd.spawn()?;
    for _ in 0..120 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if is_running(port).await {
            return Ok(());
        }
    }
    anyhow::bail!("server did not come up within 60s")
}

/// Forward a launch/install to the running server and stream its events to
/// stdout until a terminal event arrives. `json_out` passes raw event lines
/// through; otherwise they're logged human-readably.
pub async fn forward_streaming(
    port: u16,
    request: Request,
    terminal: &[&str],
    json_out: bool,
) -> Result<()> {
    let client = maxima_proto::MaximaClient::connect(port).await?;
    let mut events = client.subscribe();

    // Fire the request; a server-side error surfaces here.
    client.request(request).await?;

    loop {
        let note = match events.recv().await {
            Ok(n) => n,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        };
        let ev_name = notification_event_name(&note);
        if json_out {
            if let Ok(line) = serde_json::to_string(&note) {
                println!("{}", line);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
        } else {
            match &note {
                Notification::InstallProgress { percent, .. } => {
                    info!("Downloading: {:.1}%/100%", percent)
                }
                Notification::InstallDone { .. } => info!("Install complete."),
                Notification::InstallError { message, .. } => {
                    warn!("Install error: {}", message)
                }
                Notification::GameStarted { .. } => info!("Game started."),
                Notification::GameStopped => info!("Game stopped."),
                _ => {}
            }
        }
        if terminal.contains(&ev_name) {
            break;
        }
    }
    Ok(())
}

fn notification_event_name(note: &Notification) -> &'static str {
    match note {
        Notification::Ready { .. } => "ready",
        Notification::Presence { .. } => "presence",
        Notification::InstallProgress { .. } => "install-progress",
        Notification::InstallDone { .. } => "install-done",
        Notification::InstallError { .. } => "install-error",
        Notification::GameStarted { .. } => "game-started",
        Notification::GameStopped => "game-stopped",
        Notification::DownloadQueue { .. } => "download-queue",
    }
}
