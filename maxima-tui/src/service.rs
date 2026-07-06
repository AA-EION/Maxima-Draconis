//! TUI backend as a **true thin client** of the Maxima server.
//!
//! Holds no in-process `Maxima`: it connects to `maxima-cli server` over the
//! `maxima-proto` RPC (spawning the server if it isn't already running) and
//! renders whatever the server reports. Login, LSX, the library and RTM all
//! live in the server; this thread only forwards UI requests and relays
//! responses. Run the TUI and the egui UI at once and both reflect the same
//! session — the PR #23 promise.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use anyhow::Result;
use log::{info, warn};
use maxima_proto::{FriendDto, GameDto, MaximaClient};

pub struct InteractThreadLoginResponse {
    pub success: bool,
    pub name: String,
}

pub enum MaximaLibRequest {
    LoginRequest,
    GetGamesRequest,
    GetFriendsRequest,
    StartGameRequest(String),
    ShutdownRequest,
}

pub enum MaximaLibResponse {
    LoginResponse(InteractThreadLoginResponse),
    LoginCacheEmpty,
    GameInfoResponse(Vec<GameDto>),
    FriendInfoResponse(Vec<FriendDto>),
    InteractionThreadDiedResponse,
}

pub struct BridgeThread {
    pub rx: Receiver<MaximaLibResponse>,
    pub tx: Sender<MaximaLibRequest>,
}

impl BridgeThread {
    pub fn new() -> Self {
        let (tx0, rx1) = mpsc::channel();
        let (tx1, rx0) = mpsc::channel();

        tokio::task::spawn(async move {
            let die = tx1.clone();
            if let Err(err) = BridgeThread::run(rx1, tx1).await {
                warn!("TUI backend client failed: {}", err);
                let _ = die.send(MaximaLibResponse::InteractionThreadDiedResponse);
            } else {
                info!("TUI backend client shut down");
            }
        });

        Self { rx: rx0, tx: tx0 }
    }

    async fn run(
        rx: Receiver<MaximaLibRequest>,
        tx: Sender<MaximaLibResponse>,
    ) -> Result<()> {
        // Connect to the server, spawning `maxima-server` if it isn't up.
        let port = maxima_proto::server_port();
        let server = locate_server();
        let client: Arc<MaximaClient> = MaximaClient::connect_or_spawn(port, &server).await?;

        // The server's `ready` carries the signed-in persona.
        match client.await_ready().await {
            Ok(persona) if !persona.is_empty() => {
                tx.send(MaximaLibResponse::LoginResponse(InteractThreadLoginResponse {
                    success: true,
                    name: persona,
                }))?;
            }
            _ => {
                tx.send(MaximaLibResponse::LoginCacheEmpty)?;
            }
        }

        // Poll the UI request channel (std mpsc) without blocking the async
        // task, awaiting each RPC as it arrives.
        loop {
            match rx.try_recv() {
                Ok(req) => match req {
                    MaximaLibRequest::LoginRequest => {
                        let persona = client.persona();
                        tx.send(MaximaLibResponse::LoginResponse(InteractThreadLoginResponse {
                            success: !persona.is_empty(),
                            name: persona,
                        }))?;
                    }
                    MaximaLibRequest::GetGamesRequest => {
                        if let Ok(games) = client.list_games().await {
                            tx.send(MaximaLibResponse::GameInfoResponse(games))?;
                        }
                    }
                    MaximaLibRequest::GetFriendsRequest => {
                        if let Ok(friends) = client.friends().await {
                            tx.send(MaximaLibResponse::FriendInfoResponse(friends))?;
                        }
                    }
                    MaximaLibRequest::StartGameRequest(slug) => {
                        if let Err(err) = client.launch(&slug, vec![], None, true).await {
                            warn!("launch failed: {}", err);
                        }
                    }
                    // Disconnect only — leave the shared server running for
                    // other clients (that's the point of the server).
                    MaximaLibRequest::ShutdownRequest => break Ok(()),
                },
                Err(mpsc::TryRecvError::Empty) => {
                    if !client.is_connected() {
                        break Err(anyhow::anyhow!("server connection closed"));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(mpsc::TryRecvError::Disconnected) => break Ok(()),
            }
        }
    }
}

/// Locate `maxima-server` next to this binary (installer / cargo layout).
fn locate_server() -> std::path::PathBuf {
    #[cfg(windows)]
    const NAME: &str = "maxima-server.exe";
    #[cfg(not(windows))]
    const NAME: &str = "maxima-server";

    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join(NAME)))
        .unwrap_or_else(|| std::path::PathBuf::from(NAME))
}
