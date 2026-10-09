use crate::{bridge_thread::BackendError, GameInfo, GameSettings};
use log::{debug, error, info};
use maxima::core::{
    launch::{self, LaunchError, LaunchMode, LaunchOptions},
    LockedMaxima,
};

pub async fn start_game_request(
    maxima_arc: LockedMaxima,
    game_info: GameInfo,
    game_settings: Option<GameSettings>,
) -> Result<(), LaunchError> {
    let maxima = maxima_arc.lock().await;
    let logged_in = maxima.auth_storage().lock().await.current().is_some();
    if !logged_in {
        info!("Ignoring request to start game, not logged in.");
        return Ok(()); // TODO(headassbtw): look into if it's worth properly reporting this
    }

    debug!("got request to start game {:?}", game_info.offer);

    // This is kind of gross, but it kind of makes sense to have?
    let (exe_override, args, cloud_saves) = if let Some(settings) = game_settings {
        (
            if settings.exe_override.is_empty() {
                None
            } else {
                Some(settings.exe_override)
            },
            launch::parse_arguments(&settings.launch_args),
            settings.cloud_saves,
        )
    } else {
        (None, Vec::new(), true)
    };

    drop(maxima);

    // Pick (and, on macOS, create) THIS game's Wine prefix before anything
    // touches wine. It is carried explicitly in the launch options — license
    // dir, regedit and the spawned game all use it — rather than being
    // exported through the environment.
    #[cfg(unix)]
    let wine_prefix = Some(maxima::unix::prefix::resolve_for_game(&game_info.slug, None).await?);
    #[cfg(not(unix))]
    let wine_prefix: Option<std::path::PathBuf> = None;

    launch::start_game(
        maxima_arc.clone(),
        LaunchMode::Online(game_info.offer),
        LaunchOptions {
            path_override: exe_override,
            arguments: args,
            cloud_saves,
            // UI launches are always EA-Desktop-style; the UI never
            // receives a Steam App ID. Steam-Play handoffs come through
            // `link2ea://` to the bootstrap, not the UI's Play button.
            steam_app_id: None,
            entitlement_source: None,
            wine_prefix,
            wine_dll_overrides: Vec::new(),
        },
    )
    .await
}
