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

/// Locate the `maxima-server` binary: next to this executable (installer /
/// cargo layout) or on `PATH` as a fallback.
pub fn locate_server_binary() -> std::path::PathBuf {
    #[cfg(windows)]
    const NAME: &str = "maxima-server.exe";
    #[cfg(not(windows))]
    const NAME: &str = "maxima-server";

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(NAME);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    std::path::PathBuf::from(NAME)
}

/// Ensure a server is up, spawning `maxima-server` detached if not.
pub async fn ensure_server_running(port: u16) -> Result<()> {
    if is_running(port).await {
        return Ok(());
    }
    let bin = locate_server_binary();
    info!("No server on port {}; starting {}", port, bin.display());
    let mut cmd = std::process::Command::new(bin);
    cmd.stdin(std::process::Stdio::null())
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
