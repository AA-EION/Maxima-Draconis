use base64::{engine::general_purpose, Engine};
use derive_getters::Getters;
use log::{error, info, warn};
use std::{env, fmt::Display, path::PathBuf, sync::Arc};
use tokio::{
    process::{Child, Command},
    sync::Mutex,
};
use uuid::Uuid;

use crate::{
    core::{
        auth::{
            context::AuthContext,
            nucleus_auth_exchange,
            storage::{AuthError, TokenError},
        },
        clients::JUNO_PC_CLIENT_ID,
        cloudsync::{CloudSyncError, CloudSyncLockMode},
        library::{path_in_install_root, LibraryError, OwnedOffer},
        manifest::{self, MANIFEST_RELATIVE_PATH},
        service_layer::ServiceLayerError,
        Maxima,
    },
    ooa::{needs_license_update, request_and_save_license, LicenseAuth, LicenseError},
    steam::{load_game_overrides, override_for_offer, STEAM_APP_ID_PATTERN},
    util::{
        native::{is_wine_environment, NativeError, SafeParent, SafeStr},
        registry::bootstrap_path,
        simple_crypto,
    },
};
use thiserror::Error;

#[cfg(unix)]
use crate::unix::fs::case_insensitive_path;

use serde::{Deserialize, Serialize};

#[derive(Error, Debug)]
pub enum LaunchError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    CloudSync(#[from] CloudSyncError),
    #[error(transparent)]
    Library(#[from] LibraryError),
    #[error(transparent)]
    License(#[from] LicenseError),
    #[error(transparent)]
    Native(#[from] NativeError),
    #[error(transparent)]
    ServiceLayer(#[from] ServiceLayerError),
    #[error(transparent)]
    Token(#[from] TokenError),

    #[error("no offer was found for id `{0}`")]
    NoOfferFound(String),
    #[error("offline mode is not yet supported")]
    Offline,
    #[error("game path must be specified when launching in OnlineOffline mode")]
    GamePathOffline,
    #[error("game path not found")]
    GamePath,
    #[error("`{0}` is not installed")]
    NotInstalled(String),
    #[error("bootstrap was not found! Please re-install maxima")]
    BootstrapMissing,
    #[error(
        "content ID (`{0}`) was specified as an offer ID when launching in OnlineOffline mode"
    )]
    ContentIdAsOfferId(String),
}

pub enum StartupStage {
    Launch,
    ConnectionEstablished,
}

pub struct LibraryInjection {
    pub path: PathBuf,
    pub stage: StartupStage,
}

/// Where the game's entitlement is considered to come from. Reported to the
/// game through the `EA*Source` environment variables and the LSX
/// `EntitlementSource` / `IsSteamSubscriber` attributes, which all read it
/// from [`ActiveGameContext::entitlement_source`] so they cannot disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntitlementSource {
    Ea,
    Steam,
}

impl EntitlementSource {
    /// Explicit choice if there is one, else Steam when the launch carries a
    /// Steam App ID, else EA.
    pub fn resolve(explicit: Option<Self>, steam_app_id: Option<&str>) -> Self {
        explicit.unwrap_or(if steam_app_id.is_some() {
            Self::Steam
        } else {
            Self::Ea
        })
    }

    /// `ea` / `steam`, case-insensitive.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ea" => Some(Self::Ea),
            "steam" => Some(Self::Steam),
            _ => None,
        }
    }

    /// `MAXIMA_ENTITLEMENT_SOURCE`, if set to a valid value.
    pub fn from_env() -> Option<Self> {
        let value = env::var("MAXIMA_ENTITLEMENT_SOURCE").ok()?;
        let parsed = Self::parse(&value);
        if parsed.is_none() {
            warn!(
                "Ignoring MAXIMA_ENTITLEMENT_SOURCE='{}' (expected 'ea' or 'steam')",
                value
            );
        }
        parsed
    }

    /// Value for the `EAEntitlementSource` / `EAExternalSource` /
    /// `EALaunchOwner` environment variables.
    pub fn env_tag(self) -> &'static str {
        match self {
            Self::Ea => "EA",
            Self::Steam => "Steam",
        }
    }

    /// Value for the LSX `EntitlementSource` attribute.
    pub fn lsx_tag(self) -> &'static str {
        match self {
            Self::Ea => "EA",
            Self::Steam => "STEAM",
        }
    }
}

/// `MAXIMA_STEAM_APP_ID`, if set to a plausible Steam App ID.
pub fn steam_app_id_from_env() -> Option<String> {
    let value = env::var("MAXIMA_STEAM_APP_ID").ok()?;
    if STEAM_APP_ID_PATTERN.is_match(&value) {
        Some(value)
    } else {
        warn!("Ignoring MAXIMA_STEAM_APP_ID='{}' (expected digits only)", value);
        None
    }
}

pub struct LaunchOptions {
    pub path_override: Option<String>,
    pub arguments: Vec<String>,
    pub cloud_saves: bool,
    /// When set, the game is being launched from Steam context. Steam
    /// emits `link2ea://launchgame/<numeric_steam_app_id>?platform=steam`
    /// expecting the link2ea handler to take over the launch entirely
    /// (older EA-on-Steam titles delegate to whatever owns the link2ea
    /// protocol instead of spawning the exe themselves). Falls back to
    /// `MAXIMA_STEAM_APP_ID`.
    ///
    /// Passing `Some(steam_app_id)` causes `start_game` to:
    ///   1. Report the entitlement source as Steam (unless
    ///      `entitlement_source` says otherwise): `EAEntitlementSource` /
    ///      `EAExternalSource` / `EALaunchOwner` become `"Steam"` instead of
    ///      `"EA"` so the DRM stub sees a launch context consistent with
    ///      where it's being run from.
    ///   2. Set `SteamAppId` / `SteamGameId` env vars on the spawned game
    ///      (required by the Steam DRM stub — without these the game exits
    ///      immediately with code 100010 "Steam not detected").
    ///   3. Default `SteamClientLaunch=1` and `SteamPath=...` if the
    ///      parent env doesn't already provide them.
    ///
    /// `None` (the default) is the EA-Desktop-style launch path — env
    /// vars stay `"EA"` and no Steam-specific setup happens.
    ///
    /// Note: per-game launch args are NOT auto-injected. Callers who need
    /// them pass them via `arguments`, `MAXIMA_LAUNCH_ARGS`, or `cmd_params`
    /// on the `link2ea://` URL.
    pub steam_app_id: Option<String>,
    /// Overrides the entitlement source otherwise derived from
    /// `steam_app_id` (Steam when set, EA when not). Falls back to
    /// `MAXIMA_ENTITLEMENT_SOURCE`.
    pub entitlement_source: Option<EntitlementSource>,
    /// Wine prefix (unix) to run this one game in, overriding both the
    /// `MAXIMA_WINE_PREFIX` setting and the prefix recorded at install time
    /// (see `unix::prefix` for the precedence). `None` lets the platform
    /// pick per game. Ignored on Windows.
    pub wine_prefix: Option<PathBuf>,
    /// Extra Wine DLL overrides for this launch, each `dll[,dll]=mode`
    /// (e.g. `wsock32=n,b`), layered on top of the built-in defaults and
    /// `MAXIMA_WINE_DLL_OVERRIDES`. Ignored by hosts that don't use Wine.
    pub wine_dll_overrides: Vec<String>,
}

pub enum LaunchMode {
    /// Completely offline, relies on cached license files and user IDs
    Offline(String), // Offer ID
    /// Online, makes requests about the user and licensing
    Online(String), // Offer ID
    /// Online, but only for license requests; everything else uses dummy offer and user IDs
    /// Content ID, Game executable path, and username/password must be specified
    OnlineOffline(String, String, String), // Content ID, Persona, Password
}

impl LaunchMode {
    // What an awful name
    pub fn is_online_offline(&self) -> bool {
        match self {
            LaunchMode::OnlineOffline(_, _, _) => true,
            _ => false,
        }
    }
}

#[derive(Getters)]
pub struct ActiveGameContext {
    launch_id: String,
    game_path: String,
    content_id: String,
    offer: Option<OwnedOffer>,
    mode: LaunchMode,
    injections: Vec<LibraryInjection>,
    cloud_saves: bool,
    /// The Steam App ID this launch came from, if any. Threaded through
    /// from `LaunchOptions.steam_app_id` (the env vars it sets live on the
    /// spawned game's `Command`, not on this process).
    ///
    /// `None` means this is an EA-Desktop-style launch (a game emitting
    /// `link2ea://launchgame/Origin.OFR.…` mid-run, or maxima-cli launch
    /// with an Origin offer ID slug).
    steam_app_id: Option<String>,
    /// The game's slug, when the launch is tied to a library offer.
    slug: Option<String>,
    /// The Wine prefix this game was launched into (unix). Everything that
    /// later acts on behalf of this game (cloud-save upload, PID lookup,
    /// license requests over LSX) uses it instead of any process-wide
    /// selection.
    wine_prefix: Option<PathBuf>,
    entitlement_override: Option<EntitlementSource>,
    process: Child,
    started: bool,
}

impl ActiveGameContext {
    pub fn new(
        launch_id: &str,
        game_path: &str,
        cloud_saves: bool,
        content_id: &str,
        offer: Option<OwnedOffer>,
        mode: LaunchMode,
        steam_app_id: Option<String>,
        slug: Option<String>,
        wine_prefix: Option<PathBuf>,
        entitlement_override: Option<EntitlementSource>,
        process: Child,
    ) -> Self {
        Self {
            launch_id: launch_id.to_owned(),
            game_path: game_path.to_owned(),
            content_id: content_id.to_owned(),
            offer,
            mode,
            injections: Vec::new(),
            cloud_saves,
            steam_app_id,
            slug,
            wine_prefix,
            entitlement_override,
            process,
            started: false,
        }
    }

    /// The single source of truth for what the game is told about where its
    /// entitlement comes from.
    pub fn entitlement_source(&self) -> EntitlementSource {
        EntitlementSource::resolve(self.entitlement_override, self.steam_app_id.as_deref())
    }

    pub fn set_started(&mut self) {
        self.started = true;
    }

    pub fn process_mut(&mut self) -> &mut Child {
        &mut self.process
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct BootstrapLaunchArgs {
    pub path: String,
    pub args: Vec<String>,
    /// Wine prefix the bootstrap must run the game in (unix). Absent in
    /// payloads from older launchers, in which case the bootstrap falls back
    /// to its ambient prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wine_prefix: Option<String>,
    #[serde(default)]
    pub wine_dll_overrides: Vec<String>,
}

impl Display for LaunchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchMode::Offline(offer_id) => write!(f, "{}", offer_id),
            LaunchMode::Online(offer_id) => write!(f, "{}", offer_id),
            LaunchMode::OnlineOffline(content_id, _, _) => write!(f, "{}", content_id),
        }
    }
}

pub async fn start_game(
    maxima_arc: Arc<Mutex<Maxima>>,
    mode: LaunchMode,
    options: LaunchOptions,
) -> Result<(), LaunchError> {
    let mut maxima = maxima_arc.lock().await;
    info!("Initiating game launch with {}...", mode);

    if let LaunchMode::OnlineOffline(ref content_id, _, _) = mode {
        if options.path_override.is_none() {
            return Err(LaunchError::GamePathOffline);
        }

        if content_id.starts_with("Origin.OFR") {
            return Err(LaunchError::ContentIdAsOfferId(content_id.clone()));
        }
    }

    let (content_id, online_offline, offer, access_token) =
        if let LaunchMode::Online(ref offer_id) = mode {
            let access_token = &maxima.access_token().await?;
            let offer = match maxima.mut_library().game_by_base_offer(offer_id).await? {
                Some(offer) => offer,
                None => return Err(LaunchError::NoOfferFound(offer_id.clone())),
            };

            // Skip the EA-side install check when the caller supplied an
            // explicit path_override. This covers the Steam-launched case
            // where the game lives in Steam's library and EA Desktop has no
            // record of it — `offer.is_installed()` would return false even
            // though the binary is right there on disk.
            if options.path_override.is_none() && !offer.is_installed().await {
                return Err(LaunchError::NotInstalled(offer.offer_id().clone()));
            }

            let content_id = offer.offer().content_id().to_owned();

            (
                content_id,
                false,
                Some(offer.clone()),
                access_token.to_owned(),
            )
        } else if let LaunchMode::OnlineOffline(ref content_id, _, _) = mode {
            (content_id.to_owned(), true, None, String::new())
        } else if let LaunchMode::Offline(ref offer_id) = mode {
            // Offline: look up game from library but skip auth token
            let offer = match maxima.mut_library().game_by_base_offer(offer_id).await? {
                Some(offer) => offer,
                None => return Err(LaunchError::NoOfferFound(offer_id.clone())),
            };

            if !offer.is_installed().await {
                return Err(LaunchError::NotInstalled(offer.offer_id().clone()));
            }

            let content_id = offer.offer().content_id().to_owned();
            (content_id, false, Some(offer.clone()), String::new())
        } else {
            return Err(LaunchError::Offline);
        };

    // Need to move this into Maxima and have a "current game" system
    let path = if let Some(game_path_override) = options.path_override {
        let p = PathBuf::from(&game_path_override);
        if p.is_dir() {
            // An install directory instead of the executable: find the exe
            // from the offer's own data or the installer manifest.
            match exe_in_install_dir(&p, offer.as_ref()).await {
                Some(exe) => {
                    info!(
                        "game_path '{}' is a directory; resolved exe to '{}'",
                        p.display(),
                        exe.display()
                    );
                    exe
                }
                None => {
                    error!(
                        "game_path '{}' is a directory and the executable could not be \
                         determined for offer '{}' — pass the full path to the .exe instead.",
                        p.display(),
                        offer.as_ref().map(|o| o.offer_id().as_str()).unwrap_or("?")
                    );
                    return Err(LaunchError::GamePath);
                }
            }
        } else {
            p
        }
    } else if !online_offline {
        match offer {
            Some(ref offer) => offer.execute_path(false).await?.clone(),
            None => return Err(LaunchError::NoOfferFound("Unknown".to_string())),
        }
    } else {
        return Err(LaunchError::GamePath);
    };

    let dir = path.safe_parent()?.safe_str()?;
    #[cfg(unix)]
    let path = case_insensitive_path(path.clone());
    let path = path.safe_str()?;
    info!("Game path: {}", path);

    if is_wine_environment() {
        let path_lower = path.to_lowercase();
        if path_lower.contains("\\steamapps\\common\\")
            || path_lower.contains("/steamapps/common/")
        {
            let exe = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path);
            let slug = offer.as_ref().map(|o| o.slug().as_str()).unwrap_or("<slug>");
            warn!(
                "{} is in a Steam library. Steam DRM-wrapped executables can fail under Wine; \
                 `maxima-cli install {} --replace-files {} --only-listed-files` refreshes the \
                 executable from EA's CDN.",
                exe, slug, exe
            );
        }
    }

    // Which Wine prefix THIS game runs in. Resolved per launch and carried
    // explicitly from here on; never exported through the environment, so a
    // server launching two games in two prefixes keeps them apart.
    #[cfg(unix)]
    let wine_prefix: Option<PathBuf> = {
        let slug = offer.as_ref().map(|o| o.slug().clone());
        Some(match (slug, options.wine_prefix.as_deref()) {
            (Some(slug), explicit) => {
                crate::unix::prefix::resolve_for_game(&slug, explicit).await?
            }
            (None, Some(explicit)) => explicit.to_path_buf(),
            (None, None) => crate::unix::prefix::ambient()?,
        })
    };
    #[cfg(not(unix))]
    let wine_prefix: Option<PathBuf> = None;
    let wine_prefix_ref = wine_prefix.as_deref();

    #[cfg(unix)]
    if let Some(prefix) = wine_prefix_ref {
        mx_linux_setup(prefix).await?;
    }

    match mode {
        LaunchMode::Offline(_) => {}
        LaunchMode::Online(_) => {
            let auth = LicenseAuth::AccessToken(maxima.access_token().await?);

            let offer = offer.as_ref().unwrap();

            // Diagnostic override: setting `MAXIMA_SKIP_LICENSE_WRITE=1` in the
            // environment makes us NOT fetch + write the `.dlf` license file
            // to `…/EA Services/License/<content_id>.dlf`. Used to tell whether
            // a launch failure is driven by the on-disk `.dlf` or by something
            // else. Remove the .dlf manually before testing so there's no
            // stale file lying around.
            if env::var("MAXIMA_SKIP_LICENSE_WRITE").is_ok() {
                warn!(
                    "MAXIMA_SKIP_LICENSE_WRITE is set — skipping OOA license \
                     fetch + .dlf write entirely. Game will only have whatever \
                     .dlf was already on disk (or none)."
                );
            } else if needs_license_update(&content_id, wine_prefix_ref).await? {
                info!(
                    "Requesting new game license for {}...",
                    offer.offer().display_name()
                );

                request_and_save_license(
                    &auth,
                    &content_id,
                    path.to_owned().into(),
                    wine_prefix_ref,
                )
                .await?;
            } else {
                info!("Existing game license is still valid, not updating");
            }

            if options.cloud_saves && offer.offer().has_cloud_save() {
                info!("Syncing with cloud save...");

                let result = maxima
                    .cloud_sync()
                    .obtain_lock(offer, CloudSyncLockMode::Read, wine_prefix_ref)
                    .await;
                if let Err(err) = result {
                    error!("Failed to obtain CloudSync read lock: {}", err);
                } else {
                    let lock = result?;

                    let result = lock.sync_files().await;
                    if let Err(err) = result {
                        error!("Failed to sync cloud save: {}", err);
                    } else {
                        info!("Cloud save synced");
                    }

                    lock.release().await?;
                }
            }
        }
        LaunchMode::OnlineOffline(_, ref persona, ref password) => {
            let auth = LicenseAuth::Direct(persona.to_owned(), password.to_owned());

            if needs_license_update(&content_id, wine_prefix_ref).await? {
                request_and_save_license(
                    &auth,
                    &content_id,
                    path.to_owned().into(),
                    wine_prefix_ref,
                )
                .await?;
            } else {
                info!("Existing game license is still valid, not updating");
            }
        }
    }

    let mut game_args = options.arguments.clone();

    // Append args from env
    if let Ok(args) = env::var("MAXIMA_LAUNCH_ARGS") {
        game_args.append(&mut parse_arguments(args.as_str()));
    }

    let steam_app_id = options.steam_app_id.clone().or_else(steam_app_id_from_env);
    let entitlement_override = options
        .entitlement_source
        .or_else(EntitlementSource::from_env);
    let source_tag =
        EntitlementSource::resolve(entitlement_override, steam_app_id.as_deref()).env_tag();

    if !bootstrap_path()?.exists() {
        return Err(LaunchError::BootstrapMissing);
    }

    let slug = offer.as_ref().map(|o| o.slug().clone());

    let mut child = Command::new(bootstrap_path()?);
    child.arg("launch");

    // Detach the game tree's stdio from whatever spawned us. When Maxima
    // runs as a GUI frontend's child (ui-backend under maxima-native, or a
    // one-shot launch spawned by an app), inherited pipes/descriptors
    // connected to that app reach wine and the game — and wine's macOS
    // driver chokes on GUI-app descriptors: TF2 reproducibly freezes right
    // after LSX GetAllGameInfo. Same root cause Draconis documents in its
    // CleanSpawn service; files-or-null stdio is the shell-equivalent
    // context wine expects. Diagnostics are unaffected: bootstrap and wine
    // log to files. Windows keeps console inheritance (useful there, no
    // wine involved).
    #[cfg(unix)]
    child
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // Belt and braces for a bootstrap that predates the `wine_prefix` field
    // in its launch payload: it reads the prefix from its own environment.
    // Set on this child only, never on our own process.
    #[cfg(unix)]
    if let Some(prefix) = wine_prefix_ref {
        child.env(crate::unix::prefix::WINE_PREFIX_ENV, prefix);
    }

    let bootstrap_args = BootstrapLaunchArgs {
        path: path.to_string(),
        args: game_args,
        wine_prefix: wine_prefix
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        wine_dll_overrides: options.wine_dll_overrides.clone(),
    };

    let b64 = general_purpose::STANDARD.encode(serde_json::to_string(&bootstrap_args)?);
    child.arg(b64);

    let user = maxima.local_user().await?;
    let launch_id = Uuid::new_v4().to_string();

    // Source / owner / entitlement env vars: "EA" for EA-Desktop-launched
    // games, "Steam" for games launched via Steam. Some EA-on-Steam titles'
    // DRM stubs expect the ownership tag to match their install context.
    child
        .current_dir(PathBuf::from(path).safe_parent()?)
        .env("MXLaunchId", launch_id.to_owned())
        .env("EAAuthCode", "unavailable")
        .env("EAEgsProxyIpcPort", "0")
        .env("EAEntitlementSource", source_tag)
        .env("EAExternalSource", source_tag)
        .env("EAFreeTrialGame", "false")
        .env("EAGameLocale", maxima.locale.full_str())
        .env("EAGenericAuthToken", access_token.to_owned())
        .env("EALaunchCode", "unavailable")
        .env("EALaunchOwner", source_tag)
        .env(
            "EALaunchEAID",
            user.player()
                .as_ref()
                .ok_or(ServiceLayerError::MissingField)?
                .display_name(),
        )
        .env("EALaunchEnv", "production")
        .env("EALaunchOfflineMode", "false")
        .env("EALsxPort", maxima.effective_lsx_port().to_string())
        .env(
            "EARtPLaunchCode",
            simple_crypto::rtp_handshake().to_string(),
        )
        .env("EASecureLaunchTokenTemp", user.id())
        .env("EASteamProxyIpcPort", "0")
        .env("OriginSessionKey", launch_id.clone())
        .env("ContentId", content_id.clone())
        .env("EAOnErrorExitRetCode", "1");

    // Steam-Play env vars on the spawned child specifically (NOT via
    // `std::env::set_var` on the parent — that would persist for every
    // future spawn in the same process, which matters for the long-
    // running `maxima-cli serve` host where /authorize spawns multiple
    // games over its lifetime).
    //
    // The Steam DRM stub in EA-on-Steam titles reads `SteamAppId` /
    // `SteamGameId` during `SteamAPI_Init()`. If either is absent the
    // game exits immediately with code 100010 ("Steam not detected").
    // `SteamClientLaunch` and `SteamPath` are normally set by Steam's
    // own runtime; we default-fill them from the parent env (if Steam
    // really did launch us) or to safe constants otherwise.
    if let Some(ref app_id) = steam_app_id {
        child.env("SteamAppId", app_id).env("SteamGameId", app_id);
        let inherited_client_launch = env::var("SteamClientLaunch").ok();
        child.env(
            "SteamClientLaunch",
            inherited_client_launch.as_deref().unwrap_or("1"),
        );
        let inherited_steam_path = env::var("SteamPath").ok();
        child.env(
            "SteamPath",
            inherited_steam_path
                .as_deref()
                .unwrap_or("C:\\Program Files (x86)\\Steam"),
        );
    }

    match mode {
        LaunchMode::Offline(ref _offer_id) => {
            // Offline mode: use cached license, skip cloud sync
            // The license should already exist from a prior online session
            child.env("EALaunchOfflineMode", "true");
        }
        LaunchMode::Online(ref offer_id) => {
            // Best-effort: fetch an OPAQUE short-token for `EALaunchUserAuthToken`
            // (introduced by upstream PR #34 so the OOA license API works even
            // with a hardware-hash mismatch). Under Wine / CrossOver EA's auth
            // service routinely rejects this exchange with a redirect to
            // `signin.ea.com` (treated as `AuthError::InvalidRedirect`). When
            // that happens we fall back to the JWS access token — that's the
            // pre-PR-#34 upstream behavior and it still satisfies the env-var
            // contract the game expects. Without this fallback every launch
            // would fail end-to-end on bottles where the OOA exchange isn't
            // happy with our pc_sign / token, even though the rest of the
            // flow is fine.
            let short_token = match request_opaque_ooa_token(&access_token).await {
                Ok(token) => token,
                Err(err) => {
                    warn!(
                        "OPAQUE OOA token exchange failed ({}); falling back to \
                         JWS access_token for EALaunchUserAuthToken. The game's \
                         OOA-side calls may still work via EAAccessTokenJWS.",
                        err
                    );
                    access_token.clone()
                }
            };

            child
                .env("EAConnectionId", offer_id.clone())
                .env("EALicenseToken", offer_id.clone())
                .env("EALaunchUserAuthToken", short_token)
                .env("EAAccessTokenJWS", access_token);
        }
        LaunchMode::OnlineOffline(_, ref persona, ref password) => {
            child
                .env("EALaunchOOAUserEmail", persona)
                .env("EALaunchOOAUserPass", password)
                // Given this is probably running headlessly, don't show a UI on error
                .env("EAOnErrorExitRetCode", "1");
        }
    };

    let child = child
        .spawn()
        .map_err(|e| LaunchError::Native(NativeError::Io(e)))?;

    maxima.playing = Some(ActiveGameContext::new(
        &launch_id,
        dir,
        options.cloud_saves,
        &content_id,
        offer,
        mode,
        steam_app_id,
        slug,
        wine_prefix,
        entitlement_override,
        child,
    ));

    Ok(())
}

async fn request_opaque_ooa_token(access_token: &str) -> Result<String, AuthError> {
    let mut context = AuthContext::new()?;
    context.set_access_token(&access_token);
    context.set_token_format("OPAQUE");
    context.set_expires_in(550);

    // These scopes match the token EA Desktop requests for this
    context.add_scope("basic.commerce.cartv2");
    context.add_scope("service.atom");
    context.add_scope("dp.client.default");
    context.add_scope("signin");
    context.add_scope("social_recommendation_user");
    context.add_scope("basic.optin.write");
    context.add_scope("basic.commerce.cartv2.write");
    context.add_scope("basic.billing");
    context.add_scope("external.social_information_ups_admin");

    nucleus_auth_exchange(&context, JUNO_PC_CLIENT_ID, "token").await
}

/// Make sure the wine runtime and the given prefix are ready to run a game.
#[cfg(target_os = "linux")]
pub async fn mx_linux_setup(wine_prefix: &std::path::Path) -> Result<(), NativeError> {
    use crate::unix::wine::{
        check_runtime_validity, check_wine_validity, get_lutris_runtimes, install_runtime,
        install_wine, setup_wine_registry,
    };

    info!("Verifying wine dependencies...");

    let skip = std::env::var("MAXIMA_DISABLE_WINE_VERIFICATION").is_ok();
    if !skip {
        if !check_wine_validity().await? {
            install_wine().await?;
        }
        let runtimes = get_lutris_runtimes().await?;
        if !check_runtime_validity("eac_runtime", &runtimes).await? {
            install_runtime("eac_runtime", &runtimes).await?;
        }
        if !check_runtime_validity("umu", &runtimes).await? {
            install_runtime("umu", &runtimes).await?;
        }
    }

    std::fs::create_dir_all(wine_prefix)?;
    setup_wine_registry(wine_prefix).await?;

    Ok(())
}

/// macOS variant: games run through a CrossOver bottle — no wine/umu
/// auto-install here. The bottle is chosen (and created on demand) per game
/// by `unix::prefix::resolve_for_game`; this only checks it is really there,
/// so a wrong `--wine-prefix` fails with a clear message instead of wine
/// silently creating a fresh prefix somewhere.
#[cfg(target_os = "macos")]
pub async fn mx_linux_setup(wine_prefix: &std::path::Path) -> Result<(), NativeError> {
    use crate::unix::wine::setup_wine_registry;

    if !wine_prefix.join("system.reg").exists() {
        return Err(NativeError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "wine prefix `{}` is not an existing CrossOver bottle — `maxima-cli \
                 install`/`launch` create a per-game bottle automatically; for a custom \
                 prefix, create the bottle in CrossOver first",
                wine_prefix.display()
            ),
        )));
    }

    setup_wine_registry(wine_prefix).await?;

    Ok(())
}

/// The game's executable according to the installer manifest inside
/// `install_dir`, resolved against that directory (no registry involved).
/// The executable inside `install_dir`, trying in order the per-game
/// overrides file, the offer's execute path and the installer manifest. The
/// first candidate that exists wins; if none does the first one is returned
/// so the launch fails on a path the user can recognise.
async fn exe_in_install_dir(
    install_dir: &std::path::Path,
    offer: Option<&OwnedOffer>,
) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(offer) = offer {
        let overrides = load_game_overrides();
        if let Some(exe) = override_for_offer(&overrides, offer.offer_id()).and_then(|o| o.exe.as_ref())
        {
            candidates.push(path_in_install_root(install_dir, exe));
        }
        if let Some(name) = offer.exe_file_name().await {
            candidates.push(install_dir.join(name));
        }
    }
    if let Some(exe) = exe_from_install_manifest(install_dir).await {
        candidates.push(exe);
    }

    #[cfg(unix)]
    let exists = |p: &PathBuf| case_insensitive_path(p.clone()).exists();
    #[cfg(not(unix))]
    let exists = |p: &PathBuf| p.exists();

    candidates
        .iter()
        .find(|p| exists(p))
        .or(candidates.first())
        .cloned()
}

async fn exe_from_install_manifest(install_dir: &std::path::Path) -> Option<PathBuf> {
    let manifest_path = install_dir.join(MANIFEST_RELATIVE_PATH);
    #[cfg(unix)]
    let manifest_path = case_insensitive_path(manifest_path);
    let manifest = manifest::read(manifest_path).await.ok()?;
    let relative = manifest.execute_path(false)?;
    Some(path_in_install_root(install_dir, &relative))
}

pub fn parse_arguments(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current_arg = String::new();
    let mut in_quotes = false;

    for c in input.chars() {
        match c {
            ' ' if !in_quotes => {
                if !current_arg.is_empty() {
                    args.push(current_arg.clone());
                    current_arg.clear();
                }
            }
            '"' => {
                in_quotes = !in_quotes;
            }
            _ => {
                current_arg.push(c);
            }
        }
    }

    if !current_arg.is_empty() {
        args.push(current_arg);
    }

    args
}

#[cfg(test)]
mod wine_prefix_tests {
    use super::*;

    #[test]
    fn bootstrap_payload_from_an_older_launcher_has_no_prefix() {
        let payload = r#"{"path":"C:\\Game\\game.exe","args":["-a"]}"#;
        let args: BootstrapLaunchArgs = serde_json::from_str(payload).unwrap();
        assert_eq!(args.wine_prefix, None);
        assert_eq!(args.args, vec!["-a".to_string()]);
    }

    #[test]
    fn bootstrap_payload_carries_the_prefix_and_omits_it_when_unset() {
        let with = BootstrapLaunchArgs {
            path: "game.exe".into(),
            args: vec![],
            wine_prefix: Some("/prefixes/a".into()),
            wine_dll_overrides: vec![],
        };
        let json = serde_json::to_string(&with).unwrap();
        let back: BootstrapLaunchArgs = serde_json::from_str(&json).unwrap();
        assert_eq!(back.wine_prefix.as_deref(), Some("/prefixes/a"));

        let without = BootstrapLaunchArgs::default();
        assert!(!serde_json::to_string(&without).unwrap().contains("wine_prefix"));
    }

    #[test]
    fn parse_arguments_groups_quotes() {
        assert_eq!(
            parse_arguments(r#"-a "b c" -d"#),
            vec!["-a".to_string(), "b c".to_string(), "-d".to_string()]
        );
    }
}

#[cfg(test)]
mod entitlement_tests {
    use super::*;

    #[test]
    fn entitlement_source_resolution() {
        assert_eq!(EntitlementSource::resolve(None, None), EntitlementSource::Ea);
        assert_eq!(
            EntitlementSource::resolve(None, Some("12345")),
            EntitlementSource::Steam
        );
        assert_eq!(
            EntitlementSource::resolve(Some(EntitlementSource::Ea), Some("12345")),
            EntitlementSource::Ea
        );
        assert_eq!(
            EntitlementSource::resolve(Some(EntitlementSource::Steam), None),
            EntitlementSource::Steam
        );
    }

    #[test]
    fn entitlement_source_parsing_and_tags() {
        assert_eq!(EntitlementSource::parse(" Steam "), Some(EntitlementSource::Steam));
        assert_eq!(EntitlementSource::parse("EA"), Some(EntitlementSource::Ea));
        assert_eq!(EntitlementSource::parse("origin"), None);
        assert_eq!(EntitlementSource::Steam.env_tag(), "Steam");
        assert_eq!(EntitlementSource::Steam.lsx_tag(), "STEAM");
        assert_eq!(EntitlementSource::Ea.env_tag(), "EA");
        assert_eq!(EntitlementSource::Ea.lsx_tag(), "EA");
    }
}
