//! The CLI's **client** side of the Maxima server.
//!
//! The server itself lives in the separate `maxima-server` binary — the CLI
//! never *is* the server, it only talks to it. These helpers back
//! `server-stop` / `server-status` and the `launch`/`install` forwarding,
//! and spawn `maxima-server` on demand if nothing is listening. All of it is
//! built on `maxima_proto::MaximaClient`.

use anyhow::Result;
use log::{info, warn};
use maxima_proto::message::{Notification, Request};
use serde_json::json;
use tokio::net::TcpStream;
use tokio::sync::broadcast;

pub fn server_port() -> u16 {
    maxima_proto::server_port()
}

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
        println!("{}", json!({"running": true, "port": port, "status": status}));
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

/// Ensure a server is up, spawning `maxima-server` detached if not. Discovery
/// (sibling → App Support → PATH) is shared with every other frontend via
/// `maxima::server_client::locate_server`.
pub async fn ensure_server_running(port: u16) -> Result<()> {
    if is_running(port).await {
        return Ok(());
    }
    let bin = maxima::server_client::locate_server();
    info!("No server on port {}; starting {}", port, bin.display());
    let mut cmd = std::process::Command::new(bin);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New session so the server outlives this CLI process (and any launchd
        // app-job / terminal session it belongs to). SETSID > process group.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
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

// ---------------------------------------------------------------------------
// Pure-client command runners. Each ensures a server is up (spawning
// `maxima-server` if needed) and forwards the request — the CLI holds no
// session of its own. Streaming commands translate the server's proto
// notifications back into the exact JSONL shapes consumers (Draconis) expect.
// ---------------------------------------------------------------------------

async fn connect_ensuring(port: u16) -> Result<std::sync::Arc<maxima_proto::MaximaClient>> {
    ensure_server_running(port).await?;
    Ok(maxima_proto::MaximaClient::connect(port).await?)
}

pub async fn run_list_games(port: u16, json: bool) -> Result<()> {
    let client = connect_ensuring(port).await?;
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

pub async fn run_bottle_info(port: u16, slug: &str, json: bool) -> Result<()> {
    let client = connect_ensuring(port).await?;
    let b = client.bottle_info(slug).await?;
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
    }
    Ok(())
}

pub async fn run_locate_game(port: u16, path: &str) -> Result<()> {
    let client = connect_ensuring(port).await?;
    client.locate_game(path).await?;
    info!("Installed!");
    Ok(())
}

pub async fn run_register_protocols(port: u16) -> Result<()> {
    let client = connect_ensuring(port).await?;
    client.register_protocols().await?;
    println!("Protocol handlers registered.");
    Ok(())
}

pub async fn run_cloud_sync(port: u16, slug: &str, write: bool) -> Result<()> {
    let client = connect_ensuring(port).await?;
    client
        .request(Request::CloudSync { slug: slug.to_owned(), write })
        .await?;
    info!("Cloud sync {} done", if write { "write" } else { "read" });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_install(
    port: u16,
    slug: &str,
    path: Option<String>,
    build_id: Option<String>,
    replace_files: Vec<String>,
    only_listed_files: bool,
    json: bool,
) -> Result<()> {
    let client = connect_ensuring(port).await?;
    let mut events = client.subscribe();
    client
        .install_full(slug, path, build_id, replace_files, only_listed_files)
        .await?;
    loop {
        let note = match events.recv().await {
            Ok(n) => n,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
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
    port: u16,
    slug: &str,
    path: Option<String>,
    repair: bool,
    json: bool,
) -> Result<()> {
    let client = connect_ensuring(port).await?;
    let mut events = client.subscribe();
    client.verify(slug, path, repair).await?;
    loop {
        let note = match events.recv().await {
            Ok(n) => n,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
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
    port: u16,
    slug: &str,
    build_id: Option<String>,
    file: &str,
) -> Result<()> {
    let client = connect_ensuring(port).await?;
    client.download_file(slug, build_id, file).await?;
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
