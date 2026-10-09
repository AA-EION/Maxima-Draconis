//! `MaximaClient` — a real async client for the Maxima server. Connects over
//! loopback TCP, authenticates with the token from the server's
//! `instance.json`, correlates responses to requests by id, and exposes a
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

use crate::instance::{InstanceInfo, PROTO_VERSION};
use crate::message::{
    EntitlementSource, ErrorKind, Notification, Request, RequestEnvelope, ResponseEnvelope,
    ServerMessage,
};
use crate::types::{
    BottleInfoDto, FriendDto, GameDetailsDto, GameDto, GameImagesDto, QueueDto, StatusDto, UserDto,
};

/// Cap for ordinary requests. Long-running ones (verify, file downloads,
/// surgical installs) wait for as long as the connection stays up.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Options for [`MaximaClient::launch_with`].
#[derive(Debug, Clone)]
pub struct LaunchParams {
    pub args: Vec<String>,
    pub exe_override: Option<String>,
    pub cloud_saves: bool,
    /// Wine prefix (unix hosts); omitted = the server picks per game.
    pub wine_prefix: Option<String>,
    /// Wine DLL overrides, each `dll[,dll]=mode`.
    pub wine_dll_overrides: Vec<String>,
    pub steam_app_id: Option<String>,
    pub entitlement_source: Option<EntitlementSource>,
}

impl Default for LaunchParams {
    fn default() -> Self {
        Self {
            args: Vec::new(),
            exe_override: None,
            cloud_saves: true,
            wine_prefix: None,
            wine_dll_overrides: Vec::new(),
            steam_app_id: None,
            entitlement_source: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{message}")]
    Server { kind: ErrorKind, message: String },
    #[error("the Maxima server isn't running")]
    NotRunning,
    #[error("connection closed before response")]
    Disconnected,
    #[error("request timed out")]
    Timeout,
    #[error("malformed response (missing `{0}`)")]
    Malformed(&'static str),
}

impl ClientError {
    pub fn kind(&self) -> Option<ErrorKind> {
        match self {
            ClientError::Server { kind, .. } => Some(*kind),
            _ => None,
        }
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseEnvelope>>>>;

/// Everything an install request can carry. `Default` is "install the live
/// build to the game's default location".
#[derive(Clone, Debug, Default)]
pub struct InstallOptions {
    pub path: Option<String>,
    pub build_id: Option<String>,
    pub replace_files: Vec<String>,
    pub only_listed_files: bool,
    /// Wine prefix (unix hosts); `None` lets the server pick per game.
    pub wine_prefix: Option<String>,
    /// Glob patterns of files to leave out of the download.
    pub exclude: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum Session {
    Pending,
    Ready(String),
}

pub struct MaximaClient {
    write: Mutex<OwnedWriteHalf>,
    pending: Pending,
    next_id: AtomicU64,
    events: broadcast::Sender<Notification>,
    session: watch::Receiver<Session>,
    connected: watch::Receiver<bool>,
    realm: String,
}

impl MaximaClient {
    /// Connect to the server described by an `instance.json`.
    pub async fn connect(instance: &InstanceInfo, client: &str) -> Result<Arc<Self>, ClientError> {
        let port = instance.control_port.ok_or(ClientError::NotRunning)?;
        Self::connect_port(port, &instance.token, client).await
    }

    /// Connect to a server on an explicit port and authenticate with `token`.
    pub async fn connect_port(
        port: u16,
        token: &str,
        client_name: &str,
    ) -> Result<Arc<Self>, ClientError> {
        let stream = TcpStream::connect(("127.0.0.1", port)).await?;
        stream.set_nodelay(true).ok();
        let (read_half, write_half) = stream.into_split();

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, _) = broadcast::channel(256);
        let (session_tx, session_rx) = watch::channel(Session::Pending);
        let (conn_tx, conn_rx) = watch::channel(true);

        let reader_pending = pending.clone();
        let reader_events = events_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(read_half).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<ServerMessage>(&line) {
                    Ok(ServerMessage::Response(resp)) => {
                        if let Some(tx) = reader_pending.lock().await.remove(&resp.id) {
                            let _ = tx.send(resp);
                        }
                    }
                    Ok(ServerMessage::Notification(note)) => {
                        if let Notification::Ready { persona } = &note {
                            let _ = session_tx.send(Session::Ready(persona.clone()));
                        }
                        let _ = reader_events.send(note);
                    }
                    Err(_) => {}
                }
            }
            let _ = conn_tx.send(false);
            // Dropping the senders fails every in-flight request.
            reader_pending.lock().await.clear();
        });

        let mut this = Self {
            write: Mutex::new(write_half),
            pending,
            next_id: AtomicU64::new(1),
            events: events_tx,
            session: session_rx,
            connected: conn_rx,
            realm: String::new(),
        };
        let hello = this
            .request(Request::Hello {
                token: token.to_owned(),
                client: client_name.to_owned(),
                proto: PROTO_VERSION,
            })
            .await?;
        this.realm = hello.field("realm").unwrap_or_default();
        Ok(Arc::new(this))
    }

    /// The installation context this server belongs to.
    pub fn realm(&self) -> &str {
        &self.realm
    }

    /// Subscribe to server notifications (presence, install/game lifecycle).
    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.events.subscribe()
    }

    /// The signed-in persona, once the session is ready.
    pub fn persona(&self) -> String {
        match &*self.session.borrow() {
            Session::Ready(persona) => persona.clone(),
            Session::Pending => String::new(),
        }
    }

    /// Wait until the session is logged in (the server sends `ready`), or the
    /// connection drops. A first-run login waits on the user in a browser.
    pub async fn await_ready(&self) -> Result<String, ClientError> {
        let mut session = self.session.clone();
        let mut conn = self.connected.clone();
        loop {
            if let Session::Ready(persona) = &*session.borrow_and_update() {
                return Ok(persona.clone());
            }
            tokio::select! {
                changed = session.changed() => {
                    if changed.is_err() { return Err(ClientError::Disconnected); }
                }
                _ = conn.changed() => {
                    if !*conn.borrow() { return Err(ClientError::Disconnected); }
                }
            }
        }
    }

    /// Ask the server to log in (it opens the browser on its host).
    pub async fn login(&self) -> Result<(), ClientError> {
        self.request_raw(Request::Login).await.map(|_| ())
    }

    /// Start the login if the session isn't ready yet, then wait for it.
    pub async fn login_and_await_ready(&self) -> Result<String, ClientError> {
        if self.persona().is_empty() {
            self.login().await?;
        }
        self.await_ready().await
    }

    pub fn is_connected(&self) -> bool {
        *self.connected.borrow()
    }

    /// Resolves once the connection to the server is gone.
    pub async fn closed(&self) {
        let mut conn = self.connected.clone();
        while *conn.borrow_and_update() {
            if conn.changed().await.is_err() {
                return;
            }
        }
    }

    /// Send a request and await its matched response (30 s cap).
    pub async fn request(&self, request: Request) -> Result<ResponseEnvelope, ClientError> {
        self.request_with(request, Some(REQUEST_TIMEOUT)).await
    }

    /// Send a request and await its response, for at most `timeout` (or for
    /// as long as the connection stays up).
    pub async fn request_with(
        &self,
        request: Request,
        timeout: Option<Duration>,
    ) -> Result<ResponseEnvelope, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let line = serde_json::to_string(&RequestEnvelope { id, request })? + "\n";
        {
            let mut w = self.write.lock().await;
            w.write_all(line.as_bytes()).await?;
            w.flush().await?;
        }

        let resp = match timeout {
            Some(limit) => tokio::time::timeout(limit, rx).await.map_err(|_| {
                ClientError::Timeout
            })?,
            None => rx.await,
        }
        .map_err(|_| ClientError::Disconnected)?;

        if resp.ok {
            Ok(resp)
        } else {
            Err(ClientError::Server {
                kind: resp.kind.unwrap_or(ErrorKind::Internal),
                message: resp.error.unwrap_or_else(|| "unknown server error".into()),
            })
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
        self.launch_with(
            slug,
            LaunchParams { args, exe_override, cloud_saves, ..Default::default() },
        )
        .await
    }

    /// [`launch`](Self::launch) in an explicit Wine prefix (unix hosts).
    pub async fn launch_in(
        &self,
        slug: &str,
        args: Vec<String>,
        exe_override: Option<String>,
        cloud_saves: bool,
        wine_prefix: Option<String>,
    ) -> Result<(), ClientError> {
        self.launch_with(
            slug,
            LaunchParams { args, exe_override, cloud_saves, wine_prefix, ..Default::default() },
        )
        .await
    }

    /// Launch with the full option set.
    pub async fn launch_with(&self, slug: &str, params: LaunchParams) -> Result<(), ClientError> {
        self.request(Request::Launch {
            slug: slug.to_owned(),
            args: params.args,
            exe_override: params.exe_override,
            cloud_saves: params.cloud_saves,
            wine_prefix: params.wine_prefix,
            wine_dll_overrides: params.wine_dll_overrides,
            steam_app_id: params.steam_app_id,
            entitlement_source: params.entitlement_source,
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
        self.install_with(
            slug,
            InstallOptions {
                path,
                build_id,
                replace_files,
                only_listed_files,
                ..Default::default()
            },
        )
        .await
        .map(|_| ())
    }

    /// Install with every option, including the Wine prefix and the file
    /// exclusion patterns. Returns the server's canonical slug for the game,
    /// which keys its install notifications.
    pub async fn install_with(
        &self,
        slug: &str,
        options: InstallOptions,
    ) -> Result<String, ClientError> {
        let timeout = (!options.only_listed_files).then_some(REQUEST_TIMEOUT);
        self.request_with(
            Request::Install {
                slug: slug.to_owned(),
                path: options.path,
                build_id: options.build_id,
                replace_files: options.replace_files,
                only_listed_files: options.only_listed_files,
                wine_prefix: options.wine_prefix,
                exclude: options.exclude,
            },
            timeout,
        )
        .await
        .map(|r| r.field("slug").unwrap_or_else(|| slug.to_owned()))
    }

    pub async fn verify(
        &self,
        slug: &str,
        path: Option<String>,
        repair: bool,
    ) -> Result<(), ClientError> {
        self.verify_with(slug, path, repair, None, vec![]).await
    }

    /// [`verify`](Self::verify) with an explicit Wine prefix and extra
    /// exclusion patterns (files verify must not count as missing).
    pub async fn verify_with(
        &self,
        slug: &str,
        path: Option<String>,
        repair: bool,
        wine_prefix: Option<String>,
        exclude: Vec<String>,
    ) -> Result<(), ClientError> {
        self.request_with(
            Request::Verify { slug: slug.to_owned(), path, repair, wine_prefix, exclude },
            None,
        )
        .await
        .map(|_| ())
    }

    pub async fn download_file(
        &self,
        slug: &str,
        build_id: Option<String>,
        file: &str,
    ) -> Result<(), ClientError> {
        self.download_file_in(slug, build_id, file, None).await
    }

    pub async fn download_file_in(
        &self,
        slug: &str,
        build_id: Option<String>,
        file: &str,
        wine_prefix: Option<String>,
    ) -> Result<(), ClientError> {
        self.request_with(
            Request::DownloadFile {
                slug: slug.to_owned(),
                build_id,
                file: file.to_owned(),
                wine_prefix,
            },
            None,
        )
        .await
        .map(|_| ())
    }

    pub async fn bottle_info(&self, slug: &str) -> Result<BottleInfoDto, ClientError> {
        self.bottle_info_in(slug, None).await
    }

    pub async fn bottle_info_in(
        &self,
        slug: &str,
        wine_prefix: Option<String>,
    ) -> Result<BottleInfoDto, ClientError> {
        self.request(Request::BottleInfo { slug: slug.to_owned(), wine_prefix })
            .await?
            .field("bottle")
            .ok_or(ClientError::Malformed("bottle"))
    }

    pub async fn register_protocols(&self) -> Result<(), ClientError> {
        self.request(Request::RegisterProtocols).await.map(|_| ())
    }

    pub async fn locate_game(&self, path: &str) -> Result<(), ClientError> {
        self.locate_game_for(path, None, None).await
    }

    /// Locate an existing install, naming the game and (unix hosts) the Wine
    /// prefix it runs in.
    pub async fn locate_game_for(
        &self,
        path: &str,
        slug: Option<String>,
        wine_prefix: Option<String>,
    ) -> Result<(), ClientError> {
        self.request(Request::LocateGame { path: path.to_owned(), slug, wine_prefix })
            .await
            .map(|_| ())
    }

    pub async fn cloud_sync(&self, slug: &str, write: bool) -> Result<(), ClientError> {
        self.cloud_sync_in(slug, write, None).await
    }

    pub async fn cloud_sync_in(
        &self,
        slug: &str,
        write: bool,
        wine_prefix: Option<String>,
    ) -> Result<(), ClientError> {
        self.request_with(Request::CloudSync { slug: slug.to_owned(), write, wine_prefix }, None)
            .await
            .map(|_| ())
    }

    /// Run a queue command (`DownloadQueue`, `CancelInstall`, …) and return
    /// the queue as it stands afterwards.
    pub async fn queue(&self, request: Request) -> Result<QueueDto, ClientError> {
        self.request(request).await?.field("queue").ok_or(ClientError::Malformed("queue"))
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
