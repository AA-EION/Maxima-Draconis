//! The CLI's **client** side of the Maxima server.
//!
//! The server itself lives in the separate `maxima-server` binary — the CLI
//! never *is* the server, it only talks to it. These helpers back
//! `server-stop` / `server-status` and the `launch`/`install` forwarding,
//! and spawn `maxima-server` on demand if nothing is listening. All of it is
//! built on `maxima_proto::MaximaClient`.

use std::sync::Arc;

use anyhow::Result;
use log::{info, warn};
use maxima::server_client::{self, discover};
use maxima_proto::message::{Notification, Request};
use maxima_proto::{ClientError, Discovery, MaximaClient};
use serde_json::json;
use tokio::sync::broadcast;

const CLIENT_NAME: &str = concat!("maxima-cli/", env!("CARGO_PKG_VERSION"));

pub async fn send_shutdown() -> Result<()> {
    let client = match server_client::connect(CLIENT_NAME, false).await {
        Ok(client) => client,
        Err(ClientError::NotRunning) => {
            println!("No Maxima server running.");
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    client.shutdown().await?;
    println!("Maxima server is stopping.");
    Ok(())
}

pub async fn print_status(json_out: bool) -> Result<()> {
    let info = match discover() {
        Discovery::Running(info) => info,
        _ => {
            if json_out {
                println!("{}", json!({ "running": false }));
            } else {
                println!("Maxima server: not running.");
            }
            return Ok(());
        }
    };
    let client = MaximaClient::connect(&info, CLIENT_NAME).await?;
    let status = client.status().await?;
    if json_out {
        println!(
            "{}",
            json!({ "running": true, "port": info.control_port, "realm": info.realm, "status": status })
        );
    } else {
        println!("Maxima server: running on port {}", info.control_port.unwrap_or_default());
        println!("  realm:      {}", info.realm);
        if status.logged_in {
            println!("  persona:    {}", status.persona);
        } else {
            println!("  persona:    (waiting for the EA login)");
        }
        println!("  playing:    {}", status.playing);
        if let Some(slug) = &status.installing {
            println!("  installing: {}", slug);
        }
        println!("  clients:    {}", status.clients);
    }
    Ok(())
}

/// Connect to this context's server — spawning `maxima-server` if it isn't
/// running — and wait until its EA session is logged in.
async fn connect_ready() -> Result<Arc<MaximaClient>> {
    let client = server_client::connect(CLIENT_NAME, true).await?;
    if client.persona().is_empty() {
        info!("Waiting for the Maxima server to finish the EA login (check your browser)...");
    }
    match tokio::time::timeout(LOGIN_TIMEOUT, client.await_ready()).await {
        Ok(Ok(_)) => Ok(client),
        Ok(Err(_)) => anyhow::bail!(
            "maxima-server stopped before the login finished — login failed or was cancelled; \
             see the maxima-server log"
        ),
        Err(_) => anyhow::bail!(
            "the EA login didn't finish within {}s",
            LOGIN_TIMEOUT.as_secs()
        ),
    }
}

const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Next notification, or `None` once the server connection is gone.
async fn next_event(
    client: &MaximaClient,
    events: &mut broadcast::Receiver<Notification>,
) -> Option<Notification> {
    loop {
        tokio::select! {
            received = events.recv() => match received {
                Ok(note) => return Some(note),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            },
            _ = client.closed() => return None,
        }
    }
}

const SERVER_GONE: &str = "lost the connection to the Maxima server";

/// Forward a launch/install to the running server and stream its events to
/// stdout until a terminal event arrives. `json_out` passes raw event lines
/// through; otherwise they're logged human-readably.
pub async fn forward_streaming(request: Request, terminal: &[&str], json_out: bool) -> Result<()> {
    let client = connect_ready().await?;
    let mut events = client.subscribe();

    // Fire the request; a server-side error surfaces here.
    client.request(request).await?;

    loop {
        let Some(note) = next_event(&client, &mut events).await else {
            anyhow::bail!(SERVER_GONE);
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

// ---------------------------------------------------------------------------
// Pure-client command runners. Each ensures a server is up (spawning
// `maxima-server` if needed) and forwards the request — the CLI holds no
// session of its own. Streaming commands translate the server's proto
// notifications back into the exact JSONL shapes consumers (Draconis) expect.
// ---------------------------------------------------------------------------

pub async fn run_list_games(json: bool) -> Result<()> {
    let client = connect_ready().await?;
    let games = client.list_games().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&games)?);
    } else {
        info!("Owned games:");
        for g in &games {
            info!(
                "{:<36} - {:<36} - {:<26} - Installed: {}",
                g.slug, g.name, g.offer_id, g.installed
            );
        }
    }
    Ok(())
}

pub async fn run_bottle_info(
    slug: &str,
    json: bool,
    wine_prefix: Option<String>,
) -> Result<()> {
    let client = connect_ready().await?;
    let b = client.bottle_info_in(slug, wine_prefix).await?;
    if json {
        println!("{}", serde_json::to_string(&b)?);
    } else {
        info!("slug:             {}", b.slug);
        info!(
            "bottle:           {} (exists: {})",
            b.bottle_name.as_deref().unwrap_or("-"),
            b.wine_prefix_exists
        );
        info!("wine prefix:      {}", b.wine_prefix.as_deref().unwrap_or("-"));
        info!(
            "default game dir: {} (exists: {})",
            b.default_game_dir.as_deref().unwrap_or("-"),
            b.game_dir_exists
        );
        if let Some(source) = &b.prefix_source {
            info!("prefix chosen by: {}", source);
        }
        if let Some(dir) = &b.install_dir {
            info!("installed at:     {}", dir);
        }
        if let Some(version) = &b.version {
            info!("version:          {}", version);
        }
        if let Some(build) = &b.build_id {
            info!("build:            {}", build);
        }
    }
    Ok(())
}

pub async fn run_locate_game(
    path: &str,
    slug: Option<String>,
    wine_prefix: Option<String>,
) -> Result<()> {
    let client = connect_ready().await?;
    client.locate_game_for(path, slug, wine_prefix).await?;
    info!("Installed!");
    Ok(())
}

pub async fn run_register_protocols() -> Result<()> {
    let client = connect_ready().await?;
    client.register_protocols().await?;
    println!("Protocol handlers registered.");
    Ok(())
}

pub async fn run_cloud_sync(
    slug: &str,
    write: bool,
    wine_prefix: Option<String>,
) -> Result<()> {
    let client = connect_ready().await?;
    client
        .request(Request::CloudSync { slug: slug.to_owned(), write, wine_prefix })
        .await?;
    info!("Cloud sync {} done", if write { "write" } else { "read" });
    Ok(())
}

pub async fn run_install(
    slug: &str,
    options: maxima_proto::InstallOptions,
    json: bool,
) -> Result<()> {
    let client = connect_ready().await?;
    let mut events = client.subscribe();
    client.install_with(slug, options).await?;
    loop {
        let Some(note) = next_event(&client, &mut events).await else {
            anyhow::bail!(SERVER_GONE);
        };
        match note {
            Notification::InstallProgress { percent, .. } => {
                if json {
                    emit(&json!({"event": "progress", "percent": percent}));
                } else {
                    info!("Downloading: {:.1}%/100%", percent);
                }
            }
            Notification::InstallDone { .. } => {
                if json {
                    emit(&json!({"event": "done"}));
                } else {
                    info!("Install complete.");
                }
                break;
            }
            Notification::InstallError { message, .. } => {
                if json {
                    emit(&json!({"event": "error", "message": message}));
                }
                anyhow::bail!("{}", message);
            }
            _ => {}
        }
    }
    Ok(())
}

pub async fn run_verify(
    slug: &str,
    path: Option<String>,
    repair: bool,
    json: bool,
    wine_prefix: Option<String>,
    exclude: Vec<String>,
) -> Result<()> {
    let client = connect_ready().await?;
    let mut events = client.subscribe();
    client
        .verify_with(slug, path, repair, wine_prefix, exclude)
        .await?;
    loop {
        let Some(note) = next_event(&client, &mut events).await else {
            anyhow::bail!(SERVER_GONE);
        };
        match note {
            Notification::VerifyProgress { files_checked, total_files, .. } => {
                if json {
                    emit(&json!({"event": "progress", "phase": "verify",
                        "files_checked": files_checked, "total_files": total_files}));
                }
            }
            Notification::VerifyDone { ok, broken, repaired, .. } => {
                if json {
                    emit(&json!({"event": "verify_done", "ok": ok, "broken": broken}));
                    emit(&json!({"event": "done", "verified": ok + broken,
                        "broken": broken, "repaired": if repaired { broken } else { 0 }}));
                } else {
                    info!("Verify done — {} ok, {} broken (repaired: {})", ok, broken, repaired);
                }
                break;
            }
            Notification::VerifyError { message, .. } => {
                if json {
                    emit(&json!({"event": "error", "message": message}));
                }
                anyhow::bail!("{}", message);
            }
            _ => {}
        }
    }
    Ok(())
}

pub async fn run_download_file(
    slug: &str,
    build_id: Option<String>,
    file: &str,
    wine_prefix: Option<String>,
) -> Result<()> {
    let client = connect_ready().await?;
    client
        .download_file_in(slug, build_id, file, wine_prefix)
        .await?;
    info!("Downloaded {}", file);
    Ok(())
}

fn emit(value: &serde_json::Value) {
    println!("{}", value);
    let _ = std::io::Write::flush(&mut std::io::stdout());
}

fn notification_event_name(note: &Notification) -> &'static str {
    match note {
        Notification::Ready { .. } => "ready",
        Notification::LoginRequired => "login-required",
        Notification::Presence { .. } => "presence",
        Notification::InstallProgress { .. } => "install-progress",
        Notification::InstallDone { .. } => "install-done",
        Notification::InstallError { .. } => "install-error",
        Notification::GameStarted { .. } => "game-started",
        Notification::GameStopped => "game-stopped",
        Notification::DownloadQueue { .. } => "download-queue",
        Notification::VerifyProgress { .. } => "verify-progress",
        Notification::VerifyDone { .. } => "verify-done",
        Notification::VerifyError { .. } => "verify-error",
    }
}
