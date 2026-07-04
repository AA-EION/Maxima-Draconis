//! `maxima-cli ui-backend` — long-running JSONL backend for native UI
//! frontends (maxima-native today; any consumer tomorrow).
//!
//! One process holds the logged-in session, the LSX server, the `/authorize`
//! HTTP endpoint and the RTM connection — the same role the egui UI's
//! bridge_thread plays in-process — and speaks newline-delimited JSON over
//! stdio: requests in (each with an `id`), responses and pushed events out.
//!
//! Architecture-aligned with upstream PR #23 ("Maxima Server": all logic in
//! one server process, frontends as thin clients, states synced). That
//! branch is a stale draft whose server→client notification layer is
//! explicitly unfinished; this module is the pragmatic fork-side form of the
//! same design, notifications included. When upstream's `maxima_server`
//! matures, frontends written against this surface swap transports, not
//! concepts.
//!
//! Protocol
//! --------
//! Requests (stdin, one JSON object per line):
//!   {"id":1,"cmd":"list-games"}
//!   {"id":2,"cmd":"friends"}
//!   {"id":3,"cmd":"launch","slug":"titanfall-2","args":["-northstar"],"exe_override":null}
//!   {"id":4,"cmd":"install","slug":"titanfall-2","path":null}
//! Responses (stdout, matched by id):
//!   {"id":1,"ok":true,"games":[…]} | {"id":N,"ok":false,"error":"…"}
//! Pushed events (stdout, no id):
//!   {"event":"ready","persona":"…"}
//!   {"event":"presence","id":"…","basic":"Online","status":"…","game":…}
//!   {"event":"install-progress","slug":"…","percent":42.5}
//!   {"event":"install-done","slug":"…"} | {"event":"install-error","slug":"…","message":"…"}
//!   {"event":"game-started","slug":"…"} | {"event":"game-stopped"}
//!
//! Exit: stdin EOF (frontend went away) shuts the backend down.

use std::collections::HashMap;
use std::io::Write as _;

use anyhow::Result;
use log::{info, warn};
use maxima::core::{
    launch::{self, LaunchMode, LaunchOptions},
    LockedMaxima, MaximaEvent,
};
use maxima::rtm::client::RichPresence;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncBufReadExt;

#[derive(Deserialize)]
struct Request {
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

fn emit(value: serde_json::Value) {
    println!("{}", value);
    let _ = std::io::stdout().flush();
}

pub async fn run_ui_backend(maxima_arc: LockedMaxima) -> Result<()> {
    // --- Session setup: identical role to `serve`, plus RTM always on. ---
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

    emit(json!({"event": "ready", "persona": persona}));

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut prev_presence: HashMap<String, RichPresence> = HashMap::new();
    let mut was_playing = false;
    let mut installing: Option<String> = None;
    let mut last_percent = -1.0_f64;

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else {
                    info!("stdin closed — ui-backend shutting down");
                    break;
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Request>(&line) {
                    Ok(req) => handle_request(req, &maxima_arc, &mut installing).await,
                    Err(err) => emit(json!({
                        "event": "error",
                        "message": format!("unparseable request: {}", err),
                    })),
                }
            }
            _ = tick.tick() => {
                let mut maxima = maxima_arc.lock().await;

                for event in maxima.consume_pending_events() {
                    if let MaximaEvent::InstallFinished(offer_id) = event {
                        emit(json!({"event": "install-done", "offer_id": offer_id, "slug": installing}));
                        installing = None;
                        last_percent = -1.0;
                    }
                }

                maxima.update().await;

                let playing_now = maxima.playing().is_some();
                if was_playing && !playing_now {
                    emit(json!({"event": "game-stopped"}));
                }
                was_playing = playing_now;

                if let Some(slug) = installing.clone() {
                    match maxima.content_manager().current() {
                        Some(download) => {
                            let pct = download.percentage_done();
                            if (pct - last_percent).abs() > 0.05 {
                                emit(json!({
                                    "event": "install-progress",
                                    "slug": slug,
                                    "percent": pct,
                                }));
                                last_percent = pct;
                            }
                        }
                        None => {
                            emit(json!({"event": "install-done", "slug": slug}));
                            installing = None;
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
                        emit(json!({
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
    }

    Ok(())
}

async fn handle_request(req: Request, maxima_arc: &LockedMaxima, installing: &mut Option<String>) {
    let id = req.id;
    let result: Result<()> = match req.cmd.as_str() {
        "list-games" => cmd_list_games(id, maxima_arc).await,
        "friends" => cmd_friends(id, maxima_arc).await,
        "launch" => cmd_launch(id, req, maxima_arc).await,
        "install" => cmd_install(id, req, maxima_arc, installing).await,
        other => {
            emit(json!({"id": id, "ok": false, "error": format!("unknown cmd `{}`", other)}));
            Ok(())
        }
    };
    if let Err(err) = result {
        emit(json!({"id": id, "ok": false, "error": err.to_string()}));
    }
}

async fn cmd_list_games(id: u64, maxima_arc: &LockedMaxima) -> Result<()> {
    let mut maxima = maxima_arc.lock().await;
    let games = crate::games_json(&mut maxima).await?;
    emit(json!({"id": id, "ok": true, "games": games}));
    Ok(())
}

async fn cmd_friends(id: u64, maxima_arc: &LockedMaxima) -> Result<()> {
    let maxima = maxima_arc.lock().await;
    let friends = maxima.friends(0).await?;
    let list: Vec<serde_json::Value> = friends
        .iter()
        .map(|f| json!({"id": f.id(), "name": f.display_name()}))
        .collect();
    emit(json!({"id": id, "ok": true, "friends": list}));
    Ok(())
}

/// Resolve a typed slug to (canonical_slug, offer_id); ensures the per-game
/// bottle on macOS so everything downstream targets the right prefix.
async fn resolve_game(
    maxima_arc: &LockedMaxima,
    typed: &str,
) -> Result<(String, String)> {
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

/// The conventional per-bottle install dir when it exists (what
/// `maxima-cli install` defaults to). `launch::start_game` resolves a
/// directory to the exe via the STEAM_GAMES table.
fn conventional_game_dir(slug: &str) -> Option<String> {
    let prefix = std::env::var("MAXIMA_WINE_PREFIX").ok()?;
    let dir = std::path::Path::new(&prefix)
        .join("drive_c")
        .join("Games")
        .join(slug);
    dir.exists().then(|| dir.to_string_lossy().to_string())
}

async fn cmd_launch(id: u64, req: Request, maxima_arc: &LockedMaxima) -> Result<()> {
    let typed = req
        .slug
        .ok_or_else(|| anyhow::anyhow!("launch requires `slug`"))?;
    let (slug, offer_id) = resolve_game(maxima_arc, &typed).await?;

    let path_override = req.exe_override.or_else(|| conventional_game_dir(&slug));

    launch::start_game(
        maxima_arc.clone(),
        LaunchMode::Online(offer_id),
        LaunchOptions {
            path_override,
            arguments: req.args.unwrap_or_default(),
            cloud_saves: req.cloud_saves.unwrap_or(true),
            steam_app_id: None,
        },
    )
    .await?;

    emit(json!({"id": id, "ok": true}));
    emit(json!({"event": "game-started", "slug": slug}));
    Ok(())
}

async fn cmd_install(
    id: u64,
    req: Request,
    maxima_arc: &LockedMaxima,
    installing: &mut Option<String>,
) -> Result<()> {
    use maxima::content::manager::QueuedGameBuilder;

    if installing.is_some() {
        anyhow::bail!("another install is already running");
    }
    let typed = req
        .slug
        .ok_or_else(|| anyhow::anyhow!("install requires `slug`"))?;
    let (slug, offer_id) = resolve_game(maxima_arc, &typed).await?;

    let install_path = match req.path {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let prefix = std::env::var("MAXIMA_WINE_PREFIX").map_err(|_| {
                anyhow::anyhow!("no --path and no bottle selected for {}", slug)
            })?;
            std::path::Path::new(&prefix)
                .join("drive_c")
                .join("Games")
                .join(&slug)
        }
    };

    let mut maxima = maxima_arc.lock().await;
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

    *installing = Some(slug.clone());
    emit(json!({"id": id, "ok": true}));
    emit(json!({"event": "install-progress", "slug": slug, "percent": 0.0}));
    Ok(())
}
