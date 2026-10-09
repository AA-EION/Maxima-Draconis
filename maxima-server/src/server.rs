//! The multi-client Maxima server.
//!
//! One process holds the logged-in session, the LSX server, the `/authorize`
//! HTTP endpoint and the RTM connection, and serves **many** concurrent
//! clients over a loopback TCP socket. Every client sees the same state.
//!
//! The control port is OS-assigned (or `MAXIMA_SERVER_PORT`) and published,
//! together with a per-run token, in `instance.json` in this installation
//! context's data directory (see `maxima_proto::instance`). A connection must
//! open with a `hello` carrying that token; nothing else is answered. Wine
//! prefixes share the host loopback, so this is what keeps a client in one
//! prefix, on the host, or a web page from driving another context's session.
//!
//! The wire protocol and the client live in `maxima-proto`; this module
//! dispatches requests against the real `maxima-lib` `Maxima` and broadcasts
//! notifications to every client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use log::{error, info, warn};
use maxima::core::{
    cloudsync::CloudSyncLockMode,
    launch::{self, EntitlementSource, LaunchMode, LaunchOptions},
    manifest, LockedMaxima, Maxima, MaximaEvent,
};
use maxima::rtm::client::RichPresence;
use maxima_proto::instance::{token_matches, InstanceGuard, InstanceState, PROTO_VERSION};
use maxima_proto::message::{
    ErrorKind, Notification, Request, RequestEnvelope, ResponseEnvelope,
};
use maxima_proto::types::{
    ExtraOfferDto, FriendDto, GameDetailsDto, GameDto, QueueDto, QueueEntryDto, StatusDto,
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Mutex, Notify};

struct ServerState {
    maxima: LockedMaxima,
    realm: String,
    token: String,
    /// The EA login has finished and the session services are up.
    ready: AtomicBool,
    login_requested: Notify,
    logging_in: AtomicBool,
    /// Serialized [`Notification`] lines, broadcast to every client.
    events: broadcast::Sender<String>,
    shutdown: Notify,
    persona: Mutex<String>,
    clients: AtomicUsize,
    /// Last presence per friend, replayed to clients as they connect.
    presence: Mutex<HashMap<String, Notification>>,
}

impl ServerState {
    fn notify(&self, note: Notification) {
        // Err just means no clients are currently subscribed — fine.
        if let Ok(line) = serde_json::to_string(&note) {
            let _ = self.events.send(line);
        }
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
}

/// A request that conflicts with work already running.
#[derive(Debug)]
struct Busy(String);

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Busy {}

pub async fn run_server(maxima_arc: LockedMaxima, mut guard: InstanceGuard) -> Result<()> {
    let requested_port = std::env::var("MAXIMA_SERVER_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let listener = TcpListener::bind(("127.0.0.1", requested_port)).await?;
    let port = listener.local_addr()?.port();

    let (events_tx, _) = broadcast::channel::<String>(256);
    let state = Arc::new(ServerState {
        maxima: maxima_arc.clone(),
        realm: guard.info().realm.clone(),
        token: guard.info().token.clone(),
        ready: AtomicBool::new(false),
        login_requested: Notify::new(),
        logging_in: AtomicBool::new(false),
        events: events_tx,
        shutdown: Notify::new(),
        persona: Mutex::new(String::new()),
        clients: AtomicUsize::new(0),
        presence: Mutex::new(HashMap::new()),
    });

    // Serve before logging in: a first-run login waits on the user in the
    // browser, and clients should see `login-required` instead of a closed
    // port they'd mistake for "no server".
    guard.publish(|info| info.control_port = Some(port))?;
    info!("Maxima server listening on 127.0.0.1:{}", port);

    let accept_state = state.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let s = accept_state.clone();
                    tokio::spawn(async move { handle_client(s, stream).await });
                }
                Err(err) => warn!("accept failed: {}", err),
            }
        }
    });

    // The server owns its status-bar icon on every OS (Windows tray / macOS
    // menu-bar host / Linux SNI behind a feature). Menu: Open Maxima / Stop.
    let stop_state = state.clone();
    crate::status_icon::spawn(Arc::new(move || stop_state.shutdown.notify_one()));

    tokio::select! {
        result = start_session(&state, &mut guard) => {
            if let Err(err) = result {
                error!("Maxima session failed to start: {}", err);
                return Err(err);
            }
        }
        _ = state.shutdown.notified() => {
            info!("Shutdown requested before login finished — Maxima server stopping");
            return Ok(());
        }
    }

    let tick_state = state.clone();
    tokio::spawn(async move { tick_loop(tick_state).await });

    state.shutdown.notified().await;
    info!("Shutdown requested — Maxima server stopping");
    Ok(())
}

/// Log in, then bring up LSX, `/authorize` and RTM, and announce `ready`.
async fn start_session(state: &Arc<ServerState>, guard: &mut InstanceGuard) -> Result<()> {
    let maxima_arc = &state.maxima;
    // A client asks for the login (`login`); the server never opens a browser
    // on its own. A failed login waits for the next request.
    while !crate::saved_login(maxima_arc).await? {
        state.notify(Notification::LoginRequired);
        state.login_requested.notified().await;
        if crate::saved_login(maxima_arc).await? {
            break;
        }
        state.logging_in.store(true, Ordering::Release);
        let result = crate::oauth_login(maxima_arc).await;
        state.logging_in.store(false, Ordering::Release);
        match result {
            Ok(()) => break,
            Err(err) => {
                warn!("Login failed: {}", err);
                state.notify(Notification::LoginFailed { error: err.to_string() });
            }
        }
    }

    let (persona, lsx_port, authorize_port) = {
        let mut maxima = maxima_arc.lock().await;
        maxima.start_lsx(maxima_arc.clone()).await?;
        let authorize_port = match maxima.start_auth_server(maxima_arc.clone(), &state.token).await
        {
            Ok(port) => Some(port),
            Err(err) => {
                warn!("Authorize HTTP server failed to start: {}", err);
                None
            }
        };
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
        if let Err(err) = maxima.content_manager().start_queue().await {
            warn!("Couldn't resume the download queue: {}", err);
        }
        let user = maxima.local_user().await?;
        let persona = user
            .player()
            .as_ref()
            .map(|p| p.display_name().to_string())
            .unwrap_or_default();
        (persona, maxima.lsx_bound_port(), authorize_port)
    };

    guard.publish(|info| {
        info.state = InstanceState::Ready;
        info.lsx_port = lsx_port;
        info.authorize_port = authorize_port;
    })?;
    *state.persona.lock().await = persona.clone();
    state.ready.store(true, Ordering::Release);
    state.notify(Notification::Ready { persona: persona.clone() });
    info!("Logged in as {}; session ready", persona);
    Ok(())
}

async fn tick_loop(state: Arc<ServerState>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut prev_presence: HashMap<String, RichPresence> = HashMap::new();
    let mut was_playing = false;
    let mut last_percent = -1.0_f64;
    let mut last_queue: Option<QueueDto> = None;

    loop {
        tick.tick().await;
        let mut maxima = state.maxima.lock().await;

        let finishing = queue_snapshot(&mut maxima).current.map(|c| c.slug);
        maxima.update().await;

        for event in maxima.consume_pending_events() {
            let notification = match event {
                MaximaEvent::InstallFinished(_) => {
                    Notification::InstallDone { slug: finishing.clone() }
                }
                MaximaEvent::InstallFailed { message, .. } => {
                    Notification::InstallError { slug: finishing.clone(), message }
                }
                MaximaEvent::ReceivedLSXRequest(..) => continue,
            };
            state.notify(notification);
            last_percent = -1.0;
        }

        let playing_now = maxima.playing().is_some();
        if was_playing && !playing_now {
            state.notify(Notification::GameStopped);
        }
        was_playing = playing_now;

        let queue = queue_snapshot(&mut maxima);
        if let (Some(current), Some(download)) = (&queue.current, maxima.content_manager().current()) {
            let pct = download.percentage_done();
            if (pct - last_percent).abs() > 0.05 {
                state.notify(Notification::InstallProgress {
                    slug: current.slug.clone(),
                    percent: pct,
                    bytes: download.bytes_downloaded() as u64,
                    bytes_total: download.bytes_total() as u64,
                });
                last_percent = pct;
            }
        }
        let shape = QueueDto { percent: None, ..queue };
        if last_queue.as_ref() != Some(&shape) {
            state.notify(Notification::DownloadQueue {
                current: shape.current.as_ref().map(|c| c.slug.clone()),
                queued: shape.queued.iter().map(|q| q.slug.clone()).collect(),
                paused: shape.paused,
            });
            last_queue = Some(shape);
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
                let note = Notification::Presence {
                    id: id.clone(),
                    basic: format!("{:?}", presence.basic()),
                    status: presence.status().clone(),
                    game: presence.game().clone(),
                };
                state.presence.lock().await.insert(id.clone(), note.clone());
                state.notify(note);
                prev_presence.insert(id, presence);
            }
        }
    }
}

async fn handle_client(state: Arc<ServerState>, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // The first line must be a valid `hello`; anything else gets one error
    // reply and the connection is closed.
    let hello = match lines.next_line().await {
        Ok(Some(line)) => line,
        _ => return,
    };
    if let Err(reply) = check_hello(&state, &hello) {
        if let Ok(line) = serde_json::to_string(&reply) {
            let _ = write_half.write_all(format!("{line}\n").as_bytes()).await;
        }
        return;
    }

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

    // Subscribe before replying so no notification can fall in between.
    let mut events_rx = state.events.subscribe();
    let hello_id = id_hint(&hello);
    let reply = ResponseEnvelope::ok(
        hello_id,
        json!({ "realm": state.realm, "server": env!("CARGO_PKG_VERSION"), "proto": PROTO_VERSION }),
    );
    send(&out_tx, &reply);
    if state.is_ready() {
        let persona = state.persona.lock().await.clone();
        send(&out_tx, &Notification::Ready { persona });
        for note in state.presence.lock().await.values() {
            send(&out_tx, note);
        }
    } else {
        send(&out_tx, &Notification::LoginRequired);
    }

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

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let env = match serde_json::from_str::<RequestEnvelope>(&line) {
            Ok(env) => env,
            Err(err) => {
                let reply = ResponseEnvelope::fail(
                    id_hint(&line),
                    ErrorKind::Invalid,
                    format!("bad request: {}", err),
                );
                send(&out_tx, &reply);
                continue;
            }
        };
        // Each request runs on its own task, so a long verify or download
        // doesn't hold up this client's other requests.
        let state = state.clone();
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            let is_shutdown = matches!(env.request, Request::Shutdown);
            let response = dispatch(&state, env.id, env.request).await;
            send(&out_tx, &response);
            if is_shutdown {
                state.shutdown.notify_one();
            }
        });
    }

    forwarder.abort();
    drop(out_tx);
    let _ = writer.await;
    let remaining = state.clients.fetch_sub(1, Ordering::SeqCst) - 1;
    info!("client disconnected ({} remaining)", remaining);
}

fn send<T: serde::Serialize>(out: &mpsc::UnboundedSender<String>, message: &T) {
    if let Ok(line) = serde_json::to_string(message) {
        let _ = out.send(line);
    }
}

fn check_hello(state: &ServerState, line: &str) -> Result<(), ResponseEnvelope> {
    let id = id_hint(line);
    match serde_json::from_str::<RequestEnvelope>(line).map(|env| env.request) {
        Ok(Request::Hello { token, client, proto }) => {
            if !token_matches(&state.token, &token) {
                warn!("rejected a client with a wrong token ({})", client);
                return Err(ResponseEnvelope::fail(id, ErrorKind::Unauthorized, "bad token"));
            }
            if proto != PROTO_VERSION {
                return Err(ResponseEnvelope::fail(
                    id,
                    ErrorKind::IncompatibleVersion,
                    format!(
                        "client speaks protocol {proto}, this server (Maxima {}) speaks {PROTO_VERSION}; \
                         restart the server after updating",
                        env!("CARGO_PKG_VERSION")
                    ),
                ));
            }
            info!("client identified as {}", if client.is_empty() { "unnamed" } else { &client });
            Ok(())
        }
        _ => Err(ResponseEnvelope::fail(id, ErrorKind::Unauthorized, "send hello first")),
    }
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
    match request {
        Request::Hello { .. } => {
            return ResponseEnvelope::fail(id, ErrorKind::Invalid, "already identified")
        }
        Request::Status | Request::Shutdown => {}
        Request::Login => {
            let status = if state.is_ready() {
                "logged-in"
            } else if state.logging_in.load(Ordering::Acquire) {
                "in-progress"
            } else {
                state.login_requested.notify_one();
                "started"
            };
            return ResponseEnvelope::ok(id, json!({ "login": status }));
        }
        _ if !state.is_ready() => {
            return ResponseEnvelope::fail(
                id,
                ErrorKind::LoginPending,
                "the Maxima server is waiting for the EA login to finish",
            )
        }
        _ => {}
    }

    let result: Result<serde_json::Value> = match request {
        Request::Hello { .. } | Request::Login => unreachable!("handled above"),
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
        Request::Launch {
            slug,
            args,
            exe_override,
            cloud_saves,
            wine_prefix,
            wine_dll_overrides,
            steam_app_id,
            entitlement_source,
        } => cmd_launch(
            state,
            slug,
            args,
            exe_override,
            cloud_saves,
            wine_prefix,
            wine_dll_overrides,
            steam_app_id,
            entitlement_source,
        )
        .await
        .map(|_| json!({})),
        Request::Install {
            slug,
            path,
            build_id,
            replace_files,
            only_listed_files,
            wine_prefix,
            exclude,
        } => cmd_install(
            state,
            slug,
            path,
            build_id,
            replace_files,
            only_listed_files,
            wine_prefix,
            exclude,
        )
        .await
        .map(|slug| json!({ "slug": slug })),
        Request::LocateGame { path, slug, wine_prefix } => {
            cmd_locate(state, &path, slug, wine_prefix).await.map(|_| json!({}))
        }
        Request::CloudSync { slug, write, wine_prefix } => {
            cmd_cloud_sync(state, &slug, write, wine_prefix).await.map(|_| json!({}))
        }
        Request::Verify { slug, path, repair, wine_prefix, exclude } => {
            cmd_verify(state, slug, path, repair, wine_prefix, exclude)
                .await
                .map(|_| json!({}))
        }
        Request::DownloadFile { slug, build_id, file, wine_prefix } => {
            cmd_download_file(state, &slug, build_id, &file, wine_prefix)
                .await
                .map(|_| json!({}))
        }
        Request::BottleInfo { slug, wine_prefix } => {
            cmd_bottle_info(state, &slug, wine_prefix).await.map(|b| json!({ "bottle": b }))
        }
        Request::RegisterProtocols => cmd_register_protocols().await.map(|_| json!({})),
        Request::DownloadQueue
        | Request::CancelInstall { .. }
        | Request::PauseInstall
        | Request::ResumeInstall
        | Request::MoveInstallToTop { .. } => {
            cmd_queue(state, request).await.map(|q| json!({ "queue": q }))
        }
    };

    match result {
        Ok(data) => ResponseEnvelope::ok(id, data),
        Err(err) if err.is::<Busy>() => ResponseEnvelope::fail(id, ErrorKind::Busy, err.to_string()),
        Err(err) => ResponseEnvelope::err(id, err.to_string()),
    }
}

async fn status(state: &Arc<ServerState>) -> StatusDto {
    let logged_in = state.is_ready();
    let (playing, lsx_port) = if logged_in {
        let maxima = state.maxima.lock().await;
        (maxima.playing().is_some(), maxima.effective_lsx_port())
    } else {
        (false, 0)
    };
    StatusDto {
        persona: state.persona.lock().await.clone(),
        playing,
        installing: if logged_in {
            queue_snapshot(&mut *state.maxima.lock().await).current.map(|c| c.slug)
        } else {
            None
        },
        lsx_port,
        clients: state.clients.load(Ordering::SeqCst) as u64,
        realm: state.realm.clone(),
        logged_in,
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
        background_video: heroes
            .as_ref()
            .and_then(|h| h.items().get(0))
            .and_then(|hub| hub.background_video().as_ref())
            .and_then(|video| video.url().clone())
            .filter(|url| url.starts_with("https://") || url.starts_with("http://")),
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

/// Resolve whatever the client typed to the library's canonical
/// `(slug, offer_id)`. Pure lookup: choosing (and creating) the game's Wine
/// prefix is a separate, per-request step ([`prepare_prefix`] /
/// [`peek_prefix`]), so two games never share a selection.
async fn resolve_game(maxima_arc: &LockedMaxima, typed: &str) -> Result<(String, String)> {
    let mut maxima = maxima_arc.lock().await;
    let slug = maxima.mut_library().canonical_slug(typed).await;
    let offer_id = maxima
        .mut_library()
        .game_by_base_slug(&slug)
        .await?
        .map(|o| o.offer_id().clone())
        .ok_or_else(|| anyhow::anyhow!("`{}` is not in this EA library", typed))?;
    Ok((slug, offer_id))
}

fn explicit_prefix(wine_prefix: &Option<String>) -> Option<std::path::PathBuf> {
    wine_prefix
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(std::path::PathBuf::from)
}

/// The Wine prefix `slug` runs in for this request, created if it is Maxima's
/// to create (the per-game CrossOver bottle on macOS). Done before any lock
/// on the session is taken: creating a bottle can take a minute. `None` on
/// Windows, which has no prefixes.
#[cfg(unix)]
async fn prepare_prefix(
    slug: &str,
    wine_prefix: &Option<String>,
) -> Result<Option<std::path::PathBuf>> {
    let explicit = explicit_prefix(wine_prefix);
    Ok(Some(
        maxima::unix::prefix::resolve_for_game(slug, explicit.as_deref()).await?,
    ))
}

#[cfg(not(unix))]
async fn prepare_prefix(
    _slug: &str,
    _wine_prefix: &Option<String>,
) -> Result<Option<std::path::PathBuf>> {
    Ok(None)
}

/// Like [`prepare_prefix`] but creates nothing — for read-only commands.
#[cfg(unix)]
fn peek_prefix(
    slug: &str,
    wine_prefix: &Option<String>,
) -> Option<(std::path::PathBuf, maxima::unix::prefix::PrefixSource)> {
    let explicit = explicit_prefix(wine_prefix);
    maxima::unix::prefix::peek_for_game(slug, explicit.as_deref()).ok()
}

#[cfg(not(unix))]
fn peek_prefix(_slug: &str, _wine_prefix: &Option<String>) -> Option<(std::path::PathBuf, ())> {
    None
}

fn peeked_prefix_path(slug: &str, wine_prefix: &Option<String>) -> Option<std::path::PathBuf> {
    peek_prefix(slug, wine_prefix).map(|(path, _)| path)
}

fn recorded_install_dir(slug: &str) -> Option<std::path::PathBuf> {
    maxima::gameinfo::load_game_info(slug)
        .map(|info| info.path)
        .filter(|path| path.is_dir())
}

fn conventional_game_dir(slug: &str, prefix: Option<&std::path::Path>) -> Option<String> {
    let dir = prefix?.join("drive_c").join("Games").join(slug);
    dir.exists().then(|| dir.to_string_lossy().to_string())
}

async fn cmd_launch(
    state: &Arc<ServerState>,
    typed: String,
    args: Vec<String>,
    exe_override: Option<String>,
    cloud_saves: bool,
    wine_prefix: Option<String>,
    wine_dll_overrides: Vec<String>,
    steam_app_id: Option<String>,
    entitlement_source: Option<maxima_proto::EntitlementSource>,
) -> Result<()> {
    let (slug, offer_id) = resolve_game(&state.maxima, &typed).await?;
    let prefix = prepare_prefix(&slug, &wine_prefix).await?;
    let path_override = exe_override
        .or_else(|| recorded_install_dir(&slug).map(|d| d.to_string_lossy().to_string()))
        .or_else(|| conventional_game_dir(&slug, prefix.as_deref()));

    launch::start_game(
        state.maxima.clone(),
        LaunchMode::Online(offer_id),
        LaunchOptions {
            path_override,
            arguments: args,
            cloud_saves,
            steam_app_id,
            entitlement_source: entitlement_source.map(|s| match s {
                maxima_proto::EntitlementSource::Ea => EntitlementSource::Ea,
                maxima_proto::EntitlementSource::Steam => EntitlementSource::Steam,
            }),
            wine_prefix: prefix,
            wine_dll_overrides,
        },
    )
    .await?;

    state.notify(Notification::GameStarted { slug });
    Ok(())
}

/// Resolve the install path for a game: explicit, else where its install
/// record says it lives, else the conventional per-prefix dir.
fn install_dir_for(
    slug: &str,
    path: Option<String>,
    prefix: Option<&std::path::Path>,
) -> Result<std::path::PathBuf> {
    if let Some(p) = path {
        return Ok(std::path::PathBuf::from(p));
    }
    if let Some(dir) = recorded_install_dir(slug) {
        return Ok(dir);
    }
    let prefix = prefix
        .ok_or_else(|| anyhow::anyhow!("no path and no wine prefix known for {}", slug))?;
    Ok(prefix.join("drive_c").join("Games").join(slug))
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

#[allow(clippy::too_many_arguments)]
async fn cmd_install(
    state: &Arc<ServerState>,
    typed: String,
    path: Option<String>,
    build_id_override: Option<String>,
    replace_files: Vec<String>,
    only_listed_files: bool,
    wine_prefix: Option<String>,
    exclude: Vec<String>,
) -> Result<String> {
    use maxima::content::manager::QueuedGameBuilder;
    use maxima::content::{downloader::ZipDownloader, ContentService};

    let (slug, offer_id) = resolve_game(&state.maxima, &typed).await?;
    // A surgical refresh of a few files neither installs the game nor needs
    // its prefix to exist; a real install creates the prefix up front.
    let prefix = if only_listed_files {
        peeked_prefix_path(&slug, &wine_prefix)
    } else {
        prepare_prefix(&slug, &wine_prefix).await?
    };
    let install_path = install_dir_for(&slug, path, prefix.as_deref())?;

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
                bytes: 0,
                bytes_total: 0,
            });
            downloader.download_single_file(entry, None).await?;
        }
        state.notify(Notification::InstallProgress {
            slug: slug.clone(),
            percent: 100.0,
            bytes: 0,
            bytes_total: 0,
        });
        state.notify(Notification::InstallDone { slug: Some(slug.clone()) });
        return Ok(slug);
    }

    // Full install: queue it; the tick loop broadcasts queue and progress.
    let mut maxima = state.maxima.lock().await;
    let queue = queue_snapshot(&mut maxima);
    if queue.current.iter().chain(&queue.queued).any(|q| q.offer_id == offer_id) {
        return Ok(slug);
    }
    let locale = maxima.locale().full_str().to_owned();
    let game = QueuedGameBuilder::default()
        .offer_id(offer_id)
        .build_id(build_id)
        .path(install_path)
        .slug(slug.clone())
        .wine_prefix(prefix)
        .exclude(exclude)
        .locale(Some(locale))
        .build()?;
    maxima.content_manager().add_install(game).await?;
    Ok(slug)
}

fn queue_entry(game: &maxima::content::manager::QueuedGame) -> QueueEntryDto {
    QueueEntryDto {
        slug: if game.slug().is_empty() { game.offer_id().clone() } else { game.slug().clone() },
        offer_id: game.offer_id().clone(),
        path: game.path().to_string_lossy().into_owned(),
    }
}

fn queue_snapshot(maxima: &mut Maxima) -> QueueDto {
    let manager = maxima.content_manager();
    let queue = manager.queue();
    QueueDto {
        current: queue.current().as_ref().map(queue_entry),
        percent: manager.current().as_ref().map(|d| d.percentage_done()),
        queued: queue.queued().iter().map(queue_entry).collect(),
        paused: *queue.paused(),
    }
}

async fn cmd_queue(state: &Arc<ServerState>, request: Request) -> Result<QueueDto> {
    let mut maxima = state.maxima.lock().await;
    let offer_of = |maxima: &mut Maxima, slug: &str| -> Result<String> {
        let queue = queue_snapshot(maxima);
        queue
            .current
            .iter()
            .chain(&queue.queued)
            .find(|q| q.slug == slug || q.offer_id == slug)
            .map(|q| q.offer_id.clone())
            .ok_or_else(|| anyhow::anyhow!("'{}' is not in the download queue", slug))
    };
    match request {
        Request::CancelInstall { slug } => {
            let offer = offer_of(&mut maxima, &slug)?;
            maxima.content_manager().cancel_install(&offer).await?;
        }
        Request::MoveInstallToTop { slug } => {
            let offer = offer_of(&mut maxima, &slug)?;
            maxima.content_manager().move_install_to_top(&offer).await?;
        }
        Request::PauseInstall => maxima.content_manager().pause_install().await?,
        Request::ResumeInstall => maxima.content_manager().resume_queue().await?,
        _ => {}
    }
    Ok(queue_snapshot(&mut maxima))
}

/// Size-verify a game's files against the build manifest; `repair`
/// re-downloads the broken ones via the same replace-files primitive.
async fn cmd_verify(
    state: &Arc<ServerState>,
    typed: String,
    path: Option<String>,
    repair: bool,
    wine_prefix: Option<String>,
    exclude: Vec<String>,
) -> Result<()> {
    use maxima::content::{downloader::ZipDownloader, exclusion::get_exclusion_list, ContentService};
    use tokio::fs;

    let (slug, offer_id) = resolve_game(&state.maxima, &typed).await?;
    let prefix = peeked_prefix_path(&slug, &wine_prefix);
    let install_path = install_dir_for(&slug, path.clone(), prefix.as_deref())?;

    // Files the user excluded from the download are not "missing": the
    // game's exclusion file, what the install recorded, and this request.
    let mut patterns = maxima::gameinfo::load_game_info(&slug)
        .map(|info| info.exclude)
        .unwrap_or_default();
    patterns.extend(exclude);
    let exclusion = get_exclusion_list(&slug, &patterns);
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
    let entries: Vec<_> = downloader
        .manifest()
        .entries()
        .iter()
        .filter(|entry| !exclusion.is_match(entry.name()))
        .collect();
    let skipped = downloader.manifest().entries().len() - entries.len();
    let total = entries.len() as u64;
    info!("Verifying {} files for '{}' (build {})", total, offer_id, build_id);
    if skipped > 0 {
        info!("Skipping {} excluded file(s)", skipped);
    }

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
            wine_prefix,
            Vec::new(),
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
    wine_prefix: Option<String>,
) -> Result<()> {
    use maxima::content::{downloader::ZipDownloader, ContentService};

    let (slug, offer_id) = resolve_game(&state.maxima, typed).await?;
    let prefix = peeked_prefix_path(&slug, &wine_prefix);
    let install_path = install_dir_for(&slug, None, prefix.as_deref())?;

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
    wine_prefix: Option<String>,
) -> Result<maxima_proto::types::BottleInfoDto> {
    let slug = {
        let mut maxima = state.maxima.lock().await;
        maxima.mut_library().canonical_slug(typed).await
    };

    #[cfg(unix)]
    let (bottle_name, prefix, prefix_source): (
        Option<String>,
        Option<std::path::PathBuf>,
        Option<String>,
    ) = {
        use maxima::unix::prefix::PrefixSource;
        match peek_prefix(&slug, &wine_prefix) {
            Some((path, source)) => {
                // CrossOver addresses a bottle by name; elsewhere a name only
                // means something when the user chose the prefix themselves.
                let named = cfg!(target_os = "macos")
                    || matches!(source, PrefixSource::Explicit | PrefixSource::Override);
                let name = named
                    .then(|| maxima::unix::prefix::bottle_name(&path))
                    .flatten();
                let source = match source {
                    PrefixSource::Explicit => "explicit",
                    PrefixSource::Override => "override",
                    PrefixSource::Recorded => "recorded",
                    PrefixSource::Default => "default",
                };
                (name, Some(path), Some(source.to_owned()))
            }
            None => (None, None, None),
        }
    };
    #[cfg(windows)]
    let (bottle_name, prefix, prefix_source): (
        Option<String>,
        Option<std::path::PathBuf>,
        Option<String>,
    ) = {
        let _ = &wine_prefix;
        (None, None, None)
    };

    let record = maxima::gameinfo::load_game_info(&slug);
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
        prefix_source,
        install_dir: record.as_ref().map(|r| r.path.display().to_string()),
        build_id: record.as_ref().and_then(|r| r.build_id.clone()),
        version: record.as_ref().and_then(|r| r.version.clone()),
        locale: record.as_ref().and_then(|r| r.locale.clone()),
        installed_at: record.as_ref().and_then(|r| r.installed_at.clone()),
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

/// Register an existing install: run its touchup in the right prefix and
/// write the install record, so every later command finds it without a
/// registry.
async fn cmd_locate(
    state: &Arc<ServerState>,
    path: &str,
    slug: Option<String>,
    wine_prefix: Option<String>,
) -> Result<()> {
    use maxima::core::manifest::MANIFEST_RELATIVE_PATH;
    use maxima::gameinfo::GameInstallInfo;

    let path = std::path::PathBuf::from(path.trim_end_matches(['/', '\\']));

    // Which game is this folder? Either the client says, or we already have a
    // record for exactly this folder.
    let game = match slug {
        Some(typed) => Some(resolve_game(&state.maxima, &typed).await?),
        None => match maxima::gameinfo::find_slug_by_path(&path) {
            Some(known) => Some(resolve_game(&state.maxima, &known).await?),
            None => None,
        },
    };

    let prefix = match &game {
        Some((slug, _)) => prepare_prefix(slug, &wine_prefix).await?,
        None => {
            #[cfg(unix)]
            {
                explicit_prefix(&wine_prefix)
                    .or_else(maxima::unix::prefix::explicit_override)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "can't tell which Wine prefix `{}` belongs to — pass the game's \
                             slug (locate-game --slug) or --wine-prefix",
                            path.display()
                        )
                    })
                    .map(Some)?
            }
            #[cfg(not(unix))]
            {
                None
            }
        }
    };

    let man = manifest::read(path.join(MANIFEST_RELATIVE_PATH)).await?;
    man.run_touchup(&path, prefix.as_deref()).await?;

    if let Some((slug, offer_id)) = game {
        let locale = state.maxima.lock().await.locale().full_str().to_owned();
        let mut info = GameInstallInfo::new(path.clone(), prefix)
            .with_slug(&slug)
            .with_offer(&offer_id, None)
            .with_locale(&locale);
        info.version = man.version();
        info.save(&slug)?;
    }

    // Refresh library so the located game shows as installed to every client.
    let _ = state.maxima.lock().await.mut_library().games().await;
    Ok(())
}

async fn cmd_cloud_sync(
    state: &Arc<ServerState>,
    slug: &str,
    write: bool,
    wine_prefix: Option<String>,
) -> Result<()> {
    let explicit = explicit_prefix(&wine_prefix);
    let mut maxima = state.maxima.lock().await;
    let offer = maxima
        .mut_library()
        .game_by_base_slug(slug)
        .await?
        .ok_or_else(|| anyhow::anyhow!("`{}` not in library", slug))?
        .clone();
    let mode = if write { CloudSyncLockMode::Write } else { CloudSyncLockMode::Read };
    let lock = maxima
        .cloud_sync()
        .obtain_lock(&offer, mode, explicit.as_deref())
        .await?;
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
        let record = base.install_info();
        let downloads = base.offer().downloads();
        let live = if downloads.len() == 1 {
            downloads.first()
        } else {
            downloads.iter().find(|d| d.download_type() == "LIVE")
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
            install_dir: record.as_ref().map(|r| r.path.display().to_string()),
            wine_prefix: record
                .as_ref()
                .and_then(|r| r.wine_prefix.as_ref())
                .map(|p| p.display().to_string()),
            latest_version: live.map(|d| d.version().to_owned()),
            mandatory_update: live.map_or(false, |d| *d.treat_updates_as_mandatory()),
        });
    }
    Ok(out)
}
