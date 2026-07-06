//! `MaximaClient` — a real async client for the Maxima server. Connects over
//! loopback TCP, correlates responses to requests by id, and exposes a
//! broadcast stream of server notifications. Frontends hold one of these
//! instead of an in-process `Maxima`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, oneshot, watch, Mutex};

use crate::message::{Notification, Request, RequestEnvelope, ResponseEnvelope, ServerMessage};
use crate::types::{
    BottleInfoDto, FriendDto, GameDetailsDto, GameDto, GameImagesDto, StatusDto, UserDto,
};

pub const DEFAULT_PORT: u16 = 13220;

pub fn server_port() -> u16 {
    std::env::var("MAXIMA_SERVER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("server returned error: {0}")]
    Server(String),
    #[error("connection closed before response")]
    Disconnected,
    #[error("request timed out")]
    Timeout,
    #[error("malformed response (missing `{0}`)")]
    Malformed(&'static str),
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseEnvelope>>>>;

pub struct MaximaClient {
    write: Mutex<OwnedWriteHalf>,
    pending: Pending,
    next_id: AtomicU64,
    events: broadcast::Sender<Notification>,
    persona: watch::Receiver<String>,
    connected: watch::Receiver<bool>,
}

impl MaximaClient {
    /// Connect to a server already listening on `port`.
    pub async fn connect(port: u16) -> Result<Arc<Self>, ClientError> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await?;
        stream.set_nodelay(true).ok();
        let (read_half, write_half) = stream.into_split();

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, _) = broadcast::channel(256);
        let (persona_tx, persona_rx) = watch::channel(String::new());
        let (conn_tx, conn_rx) = watch::channel(true);

        let client = Arc::new(Self {
            write: Mutex::new(write_half),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            events: events_tx.clone(),
            persona: persona_rx,
            connected: conn_rx,
        });

        // Reader task: route responses to their oneshot, fan notifications
        // out to the broadcast channel.
        tokio::spawn(async move {
            let reader = BufReader::new(read_half);
            let mut lines = reader.lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<ServerMessage>(&line) {
                            Ok(ServerMessage::Response(resp)) => {
                                if let Some(tx) = pending.lock().await.remove(&resp.id) {
                                    let _ = tx.send(resp);
                                }
                            }
                            Ok(ServerMessage::Notification(note)) => {
                                if let Notification::Ready { persona } = &note {
                                    let _ = persona_tx.send(persona.clone());
                                }
                                let _ = events_tx.send(note);
                            }
                            Err(_) => { /* ignore unparseable lines */ }
                        }
                    }
                    _ => break, // EOF / error → disconnected
                }
            }
            let _ = conn_tx.send(false);
            // Fail any in-flight requests.
            let mut guard = pending.lock().await;
            guard.clear();
        });

        Ok(client)
    }

    /// Connect, spawning the `maxima-server` binary (at `server_path`)
    /// detached and waiting for it if nothing is listening yet.
    pub async fn connect_or_spawn(
        port: u16,
        server_path: &std::path::Path,
    ) -> Result<Arc<Self>, ClientError> {
        if let Ok(c) = Self::connect(port).await {
            return Ok(c);
        }
        let mut cmd = tokio::process::Command::new(server_path);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        cmd.process_group(0); // detach so the server outlives this client
        let _ = cmd.spawn()?;
        for _ in 0..120 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if let Ok(c) = Self::connect(port).await {
                return Ok(c);
            }
        }
        Err(ClientError::Timeout)
    }

    /// Subscribe to server notifications (presence, install/game lifecycle).
    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.events.subscribe()
    }

    /// The signed-in persona, once the server's `ready` has arrived.
    pub fn persona(&self) -> String {
        self.persona.borrow().clone()
    }

    /// Wait until the server sends `ready` (or the connection drops).
    pub async fn await_ready(&self) -> Result<String, ClientError> {
        let mut rx = self.persona.clone();
        let mut conn = self.connected.clone();
        loop {
            if !rx.borrow().is_empty() {
                return Ok(rx.borrow().clone());
            }
            tokio::select! {
                r = rx.changed() => {
                    if r.is_err() { return Err(ClientError::Disconnected); }
                    if !rx.borrow().is_empty() { return Ok(rx.borrow().clone()); }
                }
                _ = conn.changed() => {
                    if !*conn.borrow() { return Err(ClientError::Disconnected); }
                }
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }

    /// Send a request and await its matched response (30s cap).
    pub async fn request(&self, request: Request) -> Result<ResponseEnvelope, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let env = RequestEnvelope { id, request };
        let line = serde_json::to_string(&env)? + "\n";
        {
            let mut w = self.write.lock().await;
            w.write_all(line.as_bytes()).await?;
            w.flush().await?;
        }

        let resp = tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .map_err(|_| ClientError::Timeout)?
            .map_err(|_| ClientError::Disconnected)?;

        if resp.ok {
            Ok(resp)
        } else {
            Err(ClientError::Server(
                resp.error.unwrap_or_else(|| "unknown server error".into()),
            ))
        }
    }

    // Typed RPCs ---------------------------------------------------------

    pub async fn list_games(&self) -> Result<Vec<GameDto>, ClientError> {
        self.request(Request::ListGames)
            .await?
            .field("games")
            .ok_or(ClientError::Malformed("games"))
    }

    pub async fn friends(&self) -> Result<Vec<FriendDto>, ClientError> {
        self.request(Request::Friends)
            .await?
            .field("friends")
            .ok_or(ClientError::Malformed("friends"))
    }

    pub async fn status(&self) -> Result<StatusDto, ClientError> {
        self.request(Request::Status)
            .await?
            .field("status")
            .ok_or(ClientError::Malformed("status"))
    }

    pub async fn game_details(&self, slug: &str) -> Result<GameDetailsDto, ClientError> {
        self.request(Request::GameDetails { slug: slug.to_owned() })
            .await?
            .field("details")
            .ok_or(ClientError::Malformed("details"))
    }

    pub async fn whoami(&self) -> Result<UserDto, ClientError> {
        self.request(Request::WhoAmI)
            .await?
            .field("user")
            .ok_or(ClientError::Malformed("user"))
    }

    pub async fn game_images(&self, slug: &str) -> Result<GameImagesDto, ClientError> {
        self.request(Request::GameImages { slug: slug.to_owned() })
            .await?
            .field("images")
            .ok_or(ClientError::Malformed("images"))
    }

    pub async fn launch(
        &self,
        slug: &str,
        args: Vec<String>,
        exe_override: Option<String>,
        cloud_saves: bool,
    ) -> Result<(), ClientError> {
        self.request(Request::Launch {
            slug: slug.to_owned(),
            args,
            exe_override,
            cloud_saves,
        })
        .await
        .map(|_| ())
    }

    pub async fn install(&self, slug: &str, path: Option<String>) -> Result<(), ClientError> {
        self.install_full(slug, path, None, vec![], false).await
    }

    /// Install with the full option set: a specific `build_id`, a
    /// `replace_files` list (force-refresh those files of any game), and
    /// `only_listed_files` (surgical refresh, e.g. the Steam-CEG fix).
    pub async fn install_full(
        &self,
        slug: &str,
        path: Option<String>,
        build_id: Option<String>,
        replace_files: Vec<String>,
        only_listed_files: bool,
    ) -> Result<(), ClientError> {
        self.request(Request::Install {
            slug: slug.to_owned(),
            path,
            build_id,
            replace_files,
            only_listed_files,
        })
        .await
        .map(|_| ())
    }

    pub async fn verify(
        &self,
        slug: &str,
        path: Option<String>,
        repair: bool,
    ) -> Result<(), ClientError> {
        self.request(Request::Verify { slug: slug.to_owned(), path, repair })
            .await
            .map(|_| ())
    }

    pub async fn download_file(
        &self,
        slug: &str,
        build_id: Option<String>,
        file: &str,
    ) -> Result<(), ClientError> {
        self.request(Request::DownloadFile {
            slug: slug.to_owned(),
            build_id,
            file: file.to_owned(),
        })
        .await
        .map(|_| ())
    }

    pub async fn bottle_info(&self, slug: &str) -> Result<BottleInfoDto, ClientError> {
        self.request(Request::BottleInfo { slug: slug.to_owned() })
            .await?
            .field("bottle")
            .ok_or(ClientError::Malformed("bottle"))
    }

    pub async fn register_protocols(&self) -> Result<(), ClientError> {
        self.request(Request::RegisterProtocols).await.map(|_| ())
    }

    pub async fn locate_game(&self, path: &str) -> Result<(), ClientError> {
        self.request(Request::LocateGame { path: path.to_owned() })
            .await
            .map(|_| ())
    }

    pub async fn cloud_sync(&self, slug: &str, write: bool) -> Result<(), ClientError> {
        self.request(Request::CloudSync { slug: slug.to_owned(), write })
            .await
            .map(|_| ())
    }

    pub async fn shutdown(&self) -> Result<(), ClientError> {
        // The server closes the socket as it stops; a missing response is fine.
        let _ = self.request(Request::Shutdown).await;
        Ok(())
    }

    /// Raw field access for callers wanting a response field this typed API
    /// doesn't wrap yet.
    pub async fn request_raw(&self, request: Request) -> Result<Value, ClientError> {
        Ok(self.request(request).await?.data)
    }
}
