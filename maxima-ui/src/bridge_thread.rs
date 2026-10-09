use egui::Context;
use log::{info, warn};

use crate::{
    event_thread::{EventThreadFriendStatusResponse, MaximaEventResponse},
    ui_image::{UIImageCacheLoaderCommand, UIImageType},
    util::markdown::html_to_easymark,
    views::friends_view::UIFriend,
    GameDetails, GameDetailsWrapper, GameInfo, GameSettings, GameVersionInfo,
};
use maxima::{
    core::{launch::parse_arguments, manifest::MANIFEST_RELATIVE_PATH},
    rtm::client::BasicPresence,
    util::{
        native::{maxima_cache_dir, NativeError},
        registry::RegistryError,
    },
};
#[cfg(not(windows))]
use maxima::util::registry::{check_registry_validity, set_up_registry};
use maxima_proto::{
    ClientError, InstallOptions, LaunchParams, MaximaClient, Notification, QueueDto, Request,
};
use std::sync::mpsc::{Receiver, SendError, Sender, TryRecvError};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::broadcast;

pub struct InteractThreadLoginResponse {
    pub name: String,
    pub id: String,
}

pub struct InteractThreadGameListResponse {
    pub game: GameInfo,
    pub settings: GameSettings,
}

pub struct InteractThreadFriendListResponse {
    pub friend: UIFriend,
}

pub struct InteractThreadGameDetailsResponse {
    pub slug: String,
    pub response: GameDetails,
}

pub struct InteractThreadLocateGameFailure {
    pub reason: String,
    pub xml_path: String,
}

pub enum InteractThreadLocateGameResponse {
    Success,
    Error(InteractThreadLocateGameFailure),
}

pub struct InteractThreadDownloadProgressResponse {
    pub percent: f64,
    pub bytes: usize,
    pub bytes_total: usize,
}

pub enum MaximaLibRequest {
    StartService,
    LoginRequestOauth,
    GetGamesRequest,
    GetFriendsRequest,
    GetGameDetailsRequest(String),
    #[cfg(feature = "bg-videos")]
    GetGameBgVideoRequest(String),
    StartGameRequest(GameInfo, Option<GameSettings>),
    /// Offer id or slug, install folder.
    InstallGameRequest(String, PathBuf),
    /// Install folder, slug of the game it belongs to.
    LocateGameRequest(String, String),
    CancelDownload(String),
    PauseDownloads,
    ResumeDownloads,
    MoveDownloadToTop(String),
    ShutdownRequest,
    /// `maxima --install <slug> --install-path <path>`.
    AutoInstallSlug(String, PathBuf),
}

pub enum MaximaLibResponse {
    LoginResponse(Result<InteractThreadLoginResponse, anyhow::Error>),
    LoginCacheEmpty,
    ServiceNeedsStarting,
    ServiceStarted,
    GameInfoResponse(InteractThreadGameListResponse),
    FriendInfoResponse(InteractThreadFriendListResponse),
    GameDetailsResponse(InteractThreadGameDetailsResponse),
    #[cfg(feature = "bg-videos")]
    GameBgVideoResponse(String, Option<String>),
    LocateGameResponse(InteractThreadLocateGameResponse),
    CriticalError(Box<BackendError>),
    NonFatalError(Box<BackendError>),
    ActiveGameChanged(Option<String>),
    /// Slug, progress.
    DownloadProgressChanged(String, InteractThreadDownloadProgressResponse),
    DownloadFinished(String),
    /// Current slug, queued slugs, paused.
    DownloadQueueUpdate(Option<String>, Vec<String>, bool),
}

pub struct BridgeThread {
    pub backend_listener: Receiver<MaximaLibResponse>,
    pub backend_commander: Sender<MaximaLibRequest>,
    pub rtm_listener: Receiver<MaximaEventResponse>,
}

#[derive(thiserror::Error, Debug)]
pub enum BackendError {
    #[error("install of {slug} failed: {message}")]
    InstallFailed { slug: String, message: String },
    #[error(transparent)]
    Server(#[from] ClientError),
    #[error(transparent)]
    BackgroundServiceControl(#[from] maxima::util::BackgroundServiceControlError),
    #[error(transparent)]
    BackgroundServiceClient(#[from] maxima::core::error::BackgroundServiceClientError),
    #[error(transparent)]
    Native(#[from] NativeError),
    #[error(transparent)]
    RegistryError(#[from] RegistryError),
    #[error(transparent)]
    SendResponse(#[from] SendError<MaximaLibResponse>),
    #[error(transparent)]
    SendImageCacheLoaderCommand(#[from] SendError<UIImageCacheLoaderCommand>),
    #[error(transparent)]
    TryRecv(#[from] TryRecvError),

    #[error("backend-frontend communication channel disconnected")]
    ChannelDisconnected,
    #[error("lost the connection to the Maxima server")]
    ServerGone,
}

struct Ctx {
    client: Arc<MaximaClient>,
    tx: Sender<MaximaLibResponse>,
    images: Sender<UIImageCacheLoaderCommand>,
    egui: Context,
}

impl BridgeThread {
    pub fn new(ctx: &Context, remote_provider_channel: Sender<UIImageCacheLoaderCommand>) -> Self {
        puffin::profile_function!();
        let (backend_commander, backend_cmd_listener) = std::sync::mpsc::channel();
        let (backend_responder, backend_listener) = std::sync::mpsc::channel();
        let (rtm_responder, rtm_listener) = std::sync::mpsc::channel();
        let context = ctx.clone();

        tokio::task::spawn(async move {
            let die_fallback_transmitter = backend_responder.clone();
            let result = BridgeThread::run(
                backend_cmd_listener,
                backend_responder,
                rtm_responder,
                remote_provider_channel,
                &context,
            )
            .await;
            if let Err(err) = result {
                let _ = die_fallback_transmitter.send(MaximaLibResponse::CriticalError(Box::from(err)));
                context.request_repaint();
            } else {
                info!("Interact thread shut down")
            }
        });

        Self { backend_listener, backend_commander, rtm_listener }
    }

    async fn run(
        backend_cmd_listener: Receiver<MaximaLibRequest>,
        backend_responder: Sender<MaximaLibResponse>,
        rtm_responder: Sender<MaximaEventResponse>,
        remote_provider_channel: Sender<UIImageCacheLoaderCommand>,
        ctx: &Context,
    ) -> Result<(), BackendError> {
        // first things first check registry
        // the flow is different for windows/linux but windows needs an extra user prompt,
        // so we're doing both here, instead of selectively cfg'd functions!
        #[cfg(not(windows))]
        {
            if let Err(err) = check_registry_validity() {
                warn!("{}, fixing...", err);
                set_up_registry()?;
            }
        }
        #[cfg(windows)]
        {
            use maxima::{
                core::background_service::request_registry_setup,
                util::{
                    registry::check_registry_validity,
                    service::{
                        is_service_running, is_service_valid, register_service_user, start_service,
                    },
                },
            };
            if !is_elevated::is_elevated() {
                if !is_service_valid()? {
                    info!("Installing service...");
                    backend_responder.send(MaximaLibResponse::ServiceNeedsStarting)?;
                    'wait_for_user_to_authorize: loop {
                        let request = match backend_cmd_listener.try_recv() {
                            Ok(request) => request,
                            Err(TryRecvError::Empty) => {
                                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                                continue;
                            }
                            Err(TryRecvError::Disconnected) => return Ok(()),
                        };

                        match request {
                            MaximaLibRequest::StartService => {
                                register_service_user()?;
                                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                                break 'wait_for_user_to_authorize;
                            }
                            MaximaLibRequest::ShutdownRequest => return Ok(()),
                            _ => {}
                        }
                    }
                }

                if !is_service_running()? {
                    info!("Starting service...");
                    start_service().await?;
                }
            }

            if let Err(err) = check_registry_validity() {
                warn!("{}, fixing...", err);
                request_registry_setup().await?;
            }
        }
        let client = maxima::server_client::connect(
            concat!("maxima-ui/", env!("CARGO_PKG_VERSION")),
            true,
        )
        .await?;
        let mut events = client.subscribe();

        if client.persona().is_empty() {
            backend_responder.send(MaximaLibResponse::LoginCacheEmpty)?;
            ctx.request_repaint();
            'login: loop {
                match backend_cmd_listener.try_recv() {
                    Ok(MaximaLibRequest::LoginRequestOauth) => client.login().await?,
                    Ok(MaximaLibRequest::ShutdownRequest) | Err(TryRecvError::Disconnected) => {
                        return Ok(())
                    }
                    Ok(_) | Err(TryRecvError::Empty) => {}
                }
                tokio::select! {
                    note = events.recv() => match note {
                        Ok(Notification::Ready { .. }) => break 'login,
                        Ok(Notification::LoginFailed { error }) => {
                            backend_responder
                                .send(MaximaLibResponse::LoginResponse(Err(anyhow::anyhow!(error))))?;
                            backend_responder.send(MaximaLibResponse::LoginCacheEmpty)?;
                            ctx.request_repaint();
                        }
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => return Err(BackendError::ServerGone),
                    },
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
            }
        }

        let me = client.whoami().await?;
        if let Some(url) = me.avatar_url {
            remote_provider_channel
                .send(UIImageCacheLoaderCommand::ProvideRemote(UIImageType::Avatar(me.id.clone()), url))?;
        }
        backend_responder.send(MaximaLibResponse::LoginResponse(Ok(InteractThreadLoginResponse {
            name: me.name,
            id: me.id,
        })))?;
        ctx.request_repaint();

        let shared = Arc::new(Ctx {
            client: client.clone(),
            tx: backend_responder.clone(),
            images: remote_provider_channel,
            egui: ctx.clone(),
        });
        if let Ok(queue) = client.queue(Request::DownloadQueue).await {
            send_queue(&shared, queue);
        }
        tokio::spawn(pump_notifications(shared.clone(), events, rtm_responder));

        loop {
            let request = match backend_cmd_listener.try_recv() {
                Ok(MaximaLibRequest::ShutdownRequest) | Err(TryRecvError::Disconnected) => {
                    return Ok(())
                }
                Ok(request) => request,
                Err(TryRecvError::Empty) => {
                    if !client.is_connected() {
                        return Err(BackendError::ServerGone);
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    continue;
                }
            };
            let shared = shared.clone();
            tokio::spawn(async move {
                if let Err(err) = handle(&shared, request).await {
                    let _ = shared.tx.send(MaximaLibResponse::NonFatalError(Box::new(err)));
                }
                shared.egui.request_repaint();
            });
            puffin::GlobalProfiler::lock().new_frame();
        }
    }
}

fn send_queue(ctx: &Ctx, queue: QueueDto) {
    let _ = ctx.tx.send(MaximaLibResponse::DownloadQueueUpdate(
        queue.current.map(|c| c.slug),
        queue.queued.into_iter().map(|q| q.slug).collect(),
        queue.paused,
    ));
}

fn basic_presence(basic: &str) -> BasicPresence {
    match basic {
        "Online" => BasicPresence::Online,
        "Away" => BasicPresence::Away,
        "Dnd" => BasicPresence::Dnd,
        "Offline" => BasicPresence::Offline,
        _ => BasicPresence::Unknown,
    }
}

async fn pump_notifications(
    ctx: Arc<Ctx>,
    mut events: broadcast::Receiver<Notification>,
    rtm: Sender<MaximaEventResponse>,
) {
    loop {
        let note = match events.recv().await {
            Ok(note) => note,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => {
                let _ = ctx.tx.send(MaximaLibResponse::CriticalError(Box::new(BackendError::ServerGone)));
                ctx.egui.request_repaint();
                return;
            }
        };
        let response = match note {
            Notification::Presence { id, basic, status, game } => {
                let _ = rtm.send(MaximaEventResponse::FriendStatusResponse(
                    EventThreadFriendStatusResponse { id, basic: basic_presence(&basic), status, game },
                ));
                ctx.egui.request_repaint();
                continue;
            }
            Notification::GameStarted { slug } => MaximaLibResponse::ActiveGameChanged(Some(slug)),
            Notification::GameStopped => MaximaLibResponse::ActiveGameChanged(None),
            Notification::InstallProgress { slug, percent, bytes, bytes_total } => {
                MaximaLibResponse::DownloadProgressChanged(
                    slug,
                    InteractThreadDownloadProgressResponse {
                        percent,
                        bytes: bytes as usize,
                        bytes_total: bytes_total as usize,
                    },
                )
            }
            Notification::InstallDone { slug } => {
                MaximaLibResponse::DownloadFinished(slug.unwrap_or_default())
            }
            Notification::InstallError { slug, message } => MaximaLibResponse::NonFatalError(
                Box::new(BackendError::InstallFailed { slug: slug.unwrap_or_default(), message }),
            ),
            Notification::DownloadQueue { current, queued, paused } => {
                MaximaLibResponse::DownloadQueueUpdate(current, queued, paused)
            }
            _ => continue,
        };
        if ctx.tx.send(response).is_err() {
            return;
        }
        ctx.egui.request_repaint();
    }
}

async fn handle(ctx: &Arc<Ctx>, request: MaximaLibRequest) -> Result<(), BackendError> {
    let client = &ctx.client;
    match request {
        MaximaLibRequest::GetGamesRequest => {
            for game in client.list_games().await? {
                let slug = game.slug.clone();
                ctx.tx.send(MaximaLibResponse::GameInfoResponse(InteractThreadGameListResponse {
                    game: GameInfo {
                        slug: game.slug,
                        offer: game.offer_id,
                        name: game.name,
                        details: GameDetailsWrapper::Unloaded,
                        version: GameVersionInfo {
                            installed: game.version.unwrap_or_else(|| "Unknown".to_owned()),
                            latest: game.latest_version.unwrap_or_else(|| "Unknown".to_owned()),
                            mandatory: game.mandatory_update,
                        },
                        dlc: game.extra_offers,
                        installed: game.installed,
                        has_cloud_saves: game.has_cloud_save,
                    },
                    settings: GameSettings::new(),
                }))?;

                let dir = maxima_cache_dir()?.join("ui/images/").join(&slug);
                let has_hero = dir.join("hero.jpg").exists();
                let has_logo = dir.join("logo.png").exists();
                let has_background = dir.join("background.jpg").exists();
                if !(has_hero && has_logo && has_background) {
                    let ctx = ctx.clone();
                    tokio::spawn(async move {
                        let Ok(images) = ctx.client.game_images(&slug).await else { return };
                        let provide = |kind: UIImageType, url: Option<String>| {
                            let _ = ctx.images.send(match url {
                                Some(url) => UIImageCacheLoaderCommand::ProvideRemote(kind, url),
                                None => UIImageCacheLoaderCommand::Stub(kind),
                            });
                        };
                        if !has_hero && images.hero.is_some() {
                            provide(UIImageType::Hero(slug.clone()), images.hero);
                        }
                        if !has_logo {
                            provide(UIImageType::Logo(slug.clone()), images.logo);
                        }
                        if !has_background && images.background.is_some() {
                            provide(UIImageType::Background(slug.clone()), images.background);
                        }
                    });
                }
                ctx.egui.request_repaint();
            }
        }
        MaximaLibRequest::GetFriendsRequest => {
            for friend in client.friends().await? {
                if let Some(url) = friend.avatar_url {
                    ctx.images.send(UIImageCacheLoaderCommand::ProvideRemote(
                        UIImageType::Avatar(friend.id.clone()),
                        url,
                    ))?;
                }
                ctx.tx.send(MaximaLibResponse::FriendInfoResponse(InteractThreadFriendListResponse {
                    friend: UIFriend {
                        name: friend.name,
                        id: friend.id,
                        online: BasicPresence::Offline,
                        game: None,
                        game_presence: None,
                    },
                }))?;
            }
        }
        MaximaLibRequest::GetGameDetailsRequest(slug) => {
            let details = client.game_details(&slug).await?;
            ctx.tx.send(MaximaLibResponse::GameDetailsResponse(InteractThreadGameDetailsResponse {
                slug,
                response: GameDetails {
                    time: details.time,
                    achievements_unlocked: details.achievements_unlocked,
                    achievements_total: details.achievements_total,
                    path: details.path,
                    system_requirements_min: details.system_requirements_min.map(|h| html_to_easymark(&h)),
                    system_requirements_rec: details.system_requirements_rec.map(|h| html_to_easymark(&h)),
                },
            }))?;
        }
        #[cfg(feature = "bg-videos")]
        MaximaLibRequest::GetGameBgVideoRequest(slug) => {
            let url = client.game_images(&slug).await.ok().and_then(|i| i.background_video);
            ctx.tx.send(MaximaLibResponse::GameBgVideoResponse(slug, url))?;
        }
        MaximaLibRequest::StartGameRequest(info, settings) => {
            let settings = settings.unwrap_or_else(GameSettings::new);
            client
                .launch_with(
                    &info.slug,
                    LaunchParams {
                        args: parse_arguments(&settings.launch_args),
                        exe_override: (!settings.exe_override.is_empty())
                            .then_some(settings.exe_override),
                        cloud_saves: settings.cloud_saves,
                        ..Default::default()
                    },
                )
                .await?;
        }
        MaximaLibRequest::InstallGameRequest(game, path) | MaximaLibRequest::AutoInstallSlug(game, path) => {
            client
                .install_with(
                    &game,
                    InstallOptions { path: Some(path.display().to_string()), ..Default::default() },
                )
                .await?;
        }
        MaximaLibRequest::LocateGameRequest(path, slug) => {
            let path = path.trim_end_matches(['/', '\\']).to_owned();
            let response = match client.locate_game_for(&path, Some(slug), None).await {
                Ok(()) => InteractThreadLocateGameResponse::Success,
                Err(err) => InteractThreadLocateGameResponse::Error(InteractThreadLocateGameFailure {
                    reason: err.to_string(),
                    xml_path: PathBuf::from(&path).join(MANIFEST_RELATIVE_PATH).display().to_string(),
                }),
            };
            ctx.tx.send(MaximaLibResponse::LocateGameResponse(response))?;
        }
        MaximaLibRequest::CancelDownload(slug) => {
            send_queue(ctx, client.queue(Request::CancelInstall { slug }).await?)
        }
        MaximaLibRequest::PauseDownloads => send_queue(ctx, client.queue(Request::PauseInstall).await?),
        MaximaLibRequest::ResumeDownloads => send_queue(ctx, client.queue(Request::ResumeInstall).await?),
        MaximaLibRequest::MoveDownloadToTop(slug) => {
            send_queue(ctx, client.queue(Request::MoveInstallToTop { slug }).await?)
        }
        MaximaLibRequest::LoginRequestOauth
        | MaximaLibRequest::StartService
        | MaximaLibRequest::ShutdownRequest => {}
    }
    Ok(())
}
