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
    manifest, LockedMaxima, Maxima, MaximaEvent,
};
use maxima_proto::types::{ExtraOfferDto, GameDto};
use maxima::rtm::client::RichPresence;
use maxima_proto::message::{Notification, Request, RequestEnvelope, ResponseEnvelope};
use maxima_proto::types::{FriendDto, GameDetailsDto, StatusDto};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Mutex, Notify};

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

    // The server owns its status-bar icon on every OS (Windows tray / macOS
    // menu-bar host / Linux SNI behind a feature). Menu: Open Maxima / Stop.
    crate::status_icon::spawn(port);

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
            games_json(&mut maxima).await.map(|games| json!({ "games": games }))
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
        Request::Install { slug, path, build_id, replace_files, only_listed_files } => {
            cmd_install(state, slug, path, build_id, replace_files, only_listed_files)
                .await
                .map(|_| json!({}))
        }
        Request::LocateGame { path } => cmd_locate(state, &path).await.map(|_| json!({})),
        Request::CloudSync { slug, write } => {
            cmd_cloud_sync(state, &slug, write).await.map(|_| json!({}))
        }
        Request::Verify { slug, path, repair } => {
            cmd_verify(state, slug, path, repair).await.map(|_| json!({}))
        }
        Request::DownloadFile { slug, build_id, file } => {
            cmd_download_file(state, &slug, build_id, &file).await.map(|_| json!({}))
        }
        Request::BottleInfo { slug } => {
            cmd_bottle_info(state, &slug).await.map(|b| json!({ "bottle": b }))
        }
        Request::RegisterProtocols => cmd_register_protocols().await.map(|_| json!({})),
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

/// Resolve the install path for a game — explicit, or the conventional
/// per-bottle dir.
fn install_dir_for(slug: &str, path: Option<String>) -> Result<std::path::PathBuf> {
    match path {
        Some(p) => Ok(std::path::PathBuf::from(p)),
        None => {
            let prefix = std::env::var("MAXIMA_WINE_PREFIX")
                .map_err(|_| anyhow::anyhow!("no path and no bottle selected for {}", slug))?;
            Ok(std::path::Path::new(&prefix).join("drive_c").join("Games").join(slug))
        }
    }
}

/// Reject `..` / absolute segments so a bad replace-files entry can't escape
/// the install dir.
fn safe_relative(relative: &str) -> Result<()> {
    if relative
        .split(['/', '\\'])
        .any(|seg| seg == ".." || seg.is_empty())
        || std::path::PathBuf::from(relative).is_absolute()
    {
        anyhow::bail!("replace-files entries must be relative paths without '..': '{}'", relative);
    }
    Ok(())
}

async fn cmd_install(
    state: &Arc<ServerState>,
    typed: String,
    path: Option<String>,
    build_id_override: Option<String>,
    replace_files: Vec<String>,
    only_listed_files: bool,
) -> Result<()> {
    use maxima::content::manager::QueuedGameBuilder;
    use maxima::content::{downloader::ZipDownloader, ContentService};

    let (slug, offer_id) = resolve_game(&state.maxima, &typed).await?;
    let install_path = install_dir_for(&slug, path)?;

    // Pre-install replace step: delete listed files so the downloader
    // re-fetches them (works for ANY file of ANY game — the Steam-CEG fix
    // is just one caller).
    for relative in replace_files.iter().filter(|s| !s.is_empty()) {
        safe_relative(relative)?;
        let target = install_path.join(relative);
        if let Ok(meta) = std::fs::metadata(&target) {
            if meta.is_file() {
                let _ = std::fs::remove_file(&target);
            }
        }
    }

    // Resolve the build id.
    let build_id = match build_id_override {
        Some(b) => b,
        None => {
            let mut maxima = state.maxima.lock().await;
            let builds =
                maxima.content_manager().service().available_builds(&offer_id).await?;
            builds
                .live_build()
                .ok_or_else(|| anyhow::anyhow!("no live build for {}", slug))?
                .build_id()
                .to_owned()
        }
    };

    // Surgical refresh: pull ONLY the listed files from the manifest, never
    // run the full install. Runs inline (few files) and broadcasts progress.
    if only_listed_files {
        let auth = { state.maxima.lock().await.auth_storage().clone() };
        let content_service = ContentService::new(auth);
        let url = content_service.download_url(&offer_id, Some(&build_id)).await?;
        let downloader = ZipDownloader::new(&offer_id, url.url(), install_path.clone()).await?;
        let entries = downloader.manifest().entries();
        let listed: Vec<&String> = replace_files.iter().filter(|s| !s.is_empty()).collect();
        let total = listed.len().max(1);

        for (idx, relative) in listed.iter().enumerate() {
            let normalized = relative.replace('\\', "/");
            let entry = entries
                .iter()
                .find(|e| {
                    let n = e.name();
                    n.eq_ignore_ascii_case(&normalized)
                        || (n.contains('\\')
                            && n.replace('\\', "/").eq_ignore_ascii_case(&normalized))
                })
                .ok_or_else(|| {
                    anyhow::anyhow!("file '{}' not found in build {} manifest", relative, build_id)
                })?;
            state.notify(Notification::InstallProgress {
                slug: slug.clone(),
                percent: (idx as f64 / total as f64) * 100.0,
            });
            downloader.download_single_file(entry, None).await?;
        }
        state.notify(Notification::InstallProgress { slug: slug.clone(), percent: 100.0 });
        state.notify(Notification::InstallDone { slug: Some(slug) });
        return Ok(());
    }

    // Full install: queue it; the tick loop broadcasts progress.
    if state.installing.lock().await.is_some() {
        anyhow::bail!("another install is already running");
    }
    let mut maxima = state.maxima.lock().await;
    let game = QueuedGameBuilder::default()
        .offer_id(offer_id)
        .build_id(build_id)
        .path(install_path)
        .build()?;
    maxima.content_manager().install_now(game).await?;
    drop(maxima);

    *state.installing.lock().await = Some(slug.clone());
    state.notify(Notification::InstallProgress { slug: slug.clone(), percent: 0.0 });
    state.notify(Notification::DownloadQueue { current: Some(slug), queued: vec![] });
    Ok(())
}

/// Size-verify a game's files against the build manifest; `repair`
/// re-downloads the broken ones via the same replace-files primitive.
async fn cmd_verify(
    state: &Arc<ServerState>,
    typed: String,
    path: Option<String>,
    repair: bool,
) -> Result<()> {
    use maxima::content::{downloader::ZipDownloader, ContentService};
    use tokio::fs;

    let (slug, offer_id) = resolve_game(&state.maxima, &typed).await?;
    let install_path = install_dir_for(&slug, path.clone())?;
    if !fs::try_exists(&install_path).await.unwrap_or(false) {
        anyhow::bail!("install path '{}' doesn't exist", install_path.display());
    }

    let (manifest_url, build_id) = {
        let maxima = state.maxima.lock().await;
        let content_service = ContentService::new(maxima.auth_storage().clone());
        let builds = content_service.available_builds(&offer_id).await?;
        let build = builds
            .live_build()
            .ok_or_else(|| anyhow::anyhow!("no live build for {}", offer_id))?;
        let build_id = build.build_id().to_owned();
        let url = content_service.download_url(&offer_id, Some(&build_id)).await?;
        (url.url().to_owned(), build_id)
    };

    let downloader = ZipDownloader::new(&offer_id, &manifest_url, &install_path).await?;
    let entries = downloader.manifest().entries();
    let total = entries.len() as u64;
    info!("Verifying {} files for '{}' (build {})", total, offer_id, build_id);

    let mut broken: Vec<String> = Vec::new();
    let progress_every = std::cmp::max(entries.len() / 20, 100);
    for (i, entry) in entries.iter().enumerate() {
        let name = entry.name();
        if name.ends_with('/') || *entry.uncompressed_size() == 0 {
            continue;
        }
        let expected = *entry.uncompressed_size();
        let actual = fs::metadata(install_path.join(name))
            .await
            .map(|m| m.len() as i64)
            .unwrap_or(-1);
        if actual != expected {
            broken.push(name.clone());
        }
        if (i + 1) % progress_every == 0 {
            state.notify(Notification::VerifyProgress {
                slug: slug.clone(),
                files_checked: (i + 1) as u64,
                total_files: total,
            });
        }
    }

    let ok = total - broken.len() as u64;
    if broken.is_empty() {
        state.notify(Notification::VerifyDone {
            slug: slug.clone(),
            ok,
            broken: 0,
            repaired: false,
        });
        return Ok(());
    }

    if repair {
        info!("Repairing {} broken file(s)…", broken.len());
        let broken_clone = broken.clone();
        // Reuse the surgical replace path.
        Box::pin(cmd_install(
            state,
            slug.clone(),
            path,
            None,
            broken_clone,
            true,
        ))
        .await?;
        state.notify(Notification::VerifyDone {
            slug,
            ok,
            broken: broken.len() as u64,
            repaired: true,
        });
    } else {
        state.notify(Notification::VerifyDone {
            slug,
            ok,
            broken: broken.len() as u64,
            repaired: false,
        });
    }
    Ok(())
}

/// Download a single named file from a game's build manifest into its
/// install dir.
async fn cmd_download_file(
    state: &Arc<ServerState>,
    typed: &str,
    build_id: Option<String>,
    file: &str,
) -> Result<()> {
    use maxima::content::{downloader::ZipDownloader, ContentService};

    let (slug, offer_id) = resolve_game(&state.maxima, typed).await?;
    let install_path = install_dir_for(&slug, None)?;

    let auth = { state.maxima.lock().await.auth_storage().clone() };
    let content_service = ContentService::new(auth);
    let build_id = match build_id {
        Some(b) => b,
        None => {
            let builds = content_service.available_builds(&offer_id).await?;
            builds
                .live_build()
                .ok_or_else(|| anyhow::anyhow!("no live build for {}", offer_id))?
                .build_id()
                .to_owned()
        }
    };
    let url = content_service.download_url(&offer_id, Some(&build_id)).await?;
    let downloader = ZipDownloader::new(&offer_id, url.url(), install_path).await?;
    let entry = downloader
        .manifest()
        .entries()
        .iter()
        .find(|e| e.name() == file)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("file '{}' not found in build {}", file, build_id))?;
    downloader.download_single_file(&entry, None).await?;
    info!("Downloaded {} from build {}", file, build_id);
    Ok(())
}

/// Read-only bottle / prefix / game-dir readout (creates nothing).
async fn cmd_bottle_info(
    state: &Arc<ServerState>,
    typed: &str,
) -> Result<maxima_proto::types::BottleInfoDto> {
    let slug = {
        let mut maxima = state.maxima.lock().await;
        maxima.mut_library().canonical_slug(typed).await
    };

    let env_prefix = std::env::var("MAXIMA_WINE_PREFIX").ok().map(std::path::PathBuf::from);

    #[cfg(target_os = "macos")]
    let (bottle_name, prefix): (Option<String>, Option<std::path::PathBuf>) = match env_prefix {
        Some(p) => (p.file_name().map(|n| n.to_string_lossy().to_string()), Some(p)),
        None => {
            let name = format!("Maxima-{}", slug);
            let p = maxima::unix::crossover::bottles_dir().ok().map(|d| d.join(&name));
            (Some(name), p)
        }
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let (bottle_name, prefix): (Option<String>, Option<std::path::PathBuf>) = match env_prefix {
        Some(p) => (p.file_name().map(|n| n.to_string_lossy().to_string()), Some(p)),
        None => (None, maxima::unix::wine::wine_prefix_dir().ok()),
    };
    #[cfg(windows)]
    let (bottle_name, prefix): (Option<String>, Option<std::path::PathBuf>) = (None, None);

    let game_dir = prefix.as_ref().map(|p| p.join("drive_c").join("Games").join(&slug));
    let wine_prefix_exists = prefix.as_ref().map(|p| p.join("system.reg").exists()).unwrap_or(false);
    let game_dir_exists = game_dir.as_ref().map(|p| p.exists()).unwrap_or(false);

    Ok(maxima_proto::types::BottleInfoDto {
        slug,
        bottle_name,
        wine_prefix: prefix.as_ref().map(|p| p.display().to_string()),
        wine_prefix_exists,
        default_game_dir: game_dir.as_ref().map(|p| p.display().to_string()),
        game_dir_exists,
    })
}

/// Register Maxima's URL protocol handlers with the host OS.
async fn cmd_register_protocols() -> Result<()> {
    #[cfg(unix)]
    {
        maxima::util::registry::set_up_registry()?;
        info!("Protocol handlers registered");
    }
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

/// Build the machine-readable library snapshot as proto DTOs. This is the
/// server's authoritative library projection — the same data `list-games`
/// used to build in the CLI, now produced once, here, for every client.
async fn games_json(maxima: &mut Maxima) -> Result<Vec<GameDto>> {
    let titles = maxima.mut_library().games().await?;
    let mut out: Vec<GameDto> = Vec::with_capacity(titles.len());
    for title in titles {
        let base = title.base_offer();
        let installed = base.is_installed().await;
        // execute_path / installed_version read the local manifest, which can
        // be absent for externally-installed copies — swallow those (installed
        // is still meaningful; the path/version just come back null).
        let install_path = if installed {
            base.execute_path(false).await.ok().map(|p| p.display().to_string())
        } else {
            None
        };
        let version = if installed {
            base.installed_version().await.ok()
        } else {
            None
        };
        let extra_offers = title
            .extra_offers()
            .iter()
            .map(|g| ExtraOfferDto {
                offer_id: g.offer_id().clone(),
                display_name: g.offer().display_name().to_string(),
            })
            .collect();
        out.push(GameDto {
            slug: base.slug().clone(),
            name: title.name().to_string(),
            offer_id: base.offer_id().clone(),
            content_id: base.offer().content_id().to_string(),
            display_name: base.offer().display_name().to_string(),
            installed,
            install_path,
            version,
            has_cloud_save: base.offer().has_cloud_save(),
            extra_offers,
            image_url: None,
            hero_url: None,
        });
    }
    Ok(out)
}
