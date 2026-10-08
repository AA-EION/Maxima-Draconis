const LANGUAGES: &str =
    "ar_SA,de_DE,en_US,es_ES,es_MX,fr_FR,it_IT,ja_JP,ko_KR,pl_PL,pt_BR,ru_RU,zh_CN,zh_TW";
//const LANGUAGES: &str = "de_DE,en_US,es_ES,es_MX,fr_FR,it_IT,ja_JP,pl_PL,pt_BR,ru_RU,zh_TW";
//const LANGUAGES: &str = "en_US,es_ES,fr_FR,pt_BR";

use crate::{
    lsx::{
        connection::LockedConnectionState,
        request::LSXRequestError,
        types::{
            LSXGameInfoId, LSXGetAllGameInfo, LSXGetAllGameInfoResponse, LSXGetGameInfo,
            LSXGetGameInfoResponse, LSXResponseType,
        },
    },
    make_lsx_handler_response,
};

pub async fn handle_game_info_request(
    _: LockedConnectionState,
    request: LSXGetGameInfo,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    let game_info = match request.attr_GameInfoId {
        LSXGameInfoId::FreeTrial => "false".to_string(),
        LSXGameInfoId::Languages => LANGUAGES.to_string(),
        LSXGameInfoId::InstalledLanguage => "en_US".to_string(),
    };

    make_lsx_handler_response!(Response, GetGameInfoResponse, { attr_GameInfo: game_info })
}

// Sample EA Desktop response (from a real Battlefield V LSX trace) for
// reference of the field shape — DO NOT use these values, they're stale:
// <GetAllGameInfoResponse FullGamePurchased="true" FullGameReleased="true"
//   InstalledVersion="0" MaxGroupSize="16" Languages="..."
//   Expiration="0000-00-00T00:00:00" UpToDate="true" HasExpiration="false"
//   InstalledLanguage="" EntitlementSource="STEAM"
//   FullGameReleaseDate="2020-10-22T09:00:00" AvailableVersion="1.0.64.43203"
//   DisplayName="Battlefield V Definitive Edition" FreeTrial="false"
//   SystemTime="2023-06-23T04:22:10"/>

const UNKNOWN_RELEASE_DATE: &str = "0000-00-00T00:00:00";

/// Handles `GetAllGameInfo` — the LSX request the game uses to verify that
/// the auth server's view of "what's installed" matches what's on disk.
///
/// Everything here is about the game that is actually running: the display
/// name and versions come from the active offer's library data when Maxima
/// launched the game, else from what the client reported in the LSX
/// challenge (`Version` / `Title`, captured via `set_game_metadata`), else
/// they are left empty. Nothing in this handler is specific to one title.
///
/// `InstalledVersion` and `AvailableVersion` need to agree with the client's
/// own idea of its version: some games (Titanfall 2 among them) treat a
/// mismatch as tampering and raise an "Engine Error: File corruption
/// detected" dialog.
pub async fn handle_all_game_info_request(
    state: LockedConnectionState,
    _: LSXGetAllGameInfo,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    let (challenge_version, challenge_title) = {
        let s = state.read().await;
        (s.game_version().clone(), s.game_title().clone())
    };

    // `EntitlementSource` must agree with `IsSteamSubscriber` in
    // `GetProfileResponse` — a contradiction (e.g. "STEAM" +
    // IsSteamSubscriber=false) is read as a tamper signal by some games' DRM
    // stubs. Both are sourced from `ActiveGameContext.steam_app_id`, the
    // Steam App ID that triggered this launch, if any; anything else is an
    // EA launch.
    let (offer, is_steam) = {
        let arc = state.write().await.maxima_arc();
        let maxima = arc.lock().await;
        let playing = maxima.playing().as_ref();
        (
            playing.and_then(|p| p.offer().clone()),
            playing.is_some_and(|p| p.steam_app_id().is_some()),
        )
    };

    // The offer is a clone, so the Maxima lock is not held while reading the
    // install manifest from disk.
    let (display_name, installed_version, available_version, release_date) = match &offer {
        Some(offer) => {
            let download = offer.offer().downloads().first();
            (
                Some(offer.offer().display_name().to_owned()),
                offer.installed_version().await.ok(),
                download.and_then(|d| d.game_version().clone()),
                download.map(|d| d.build_live_date().clone()),
            )
        }
        None => (None, None, None, None),
    };

    let display_name = display_name.or(challenge_title).unwrap_or_default();
    let installed_version = installed_version
        .or_else(|| challenge_version.clone())
        .unwrap_or_default();
    let available_version = available_version
        .or(challenge_version)
        .unwrap_or_default();

    make_lsx_handler_response!(Response, GetAllGameInfoResponse, {
        attr_FullGamePurchased: true,
        attr_FullGameReleased: true,
        attr_InstalledVersion: installed_version,
        attr_MaxGroupSize: 16,
        attr_Languages: LANGUAGES.to_string(),
        attr_Expiration: "0000-00-00T00:00:00".to_string(),
        attr_UpToDate: true,
        attr_HasExpiration: false,
        attr_EntitlementSource: if is_steam { "STEAM" } else { "EA" }.to_string(),
        attr_AvailableVersion: available_version,
        attr_DisplayName: display_name,
        attr_FreeTrial: false,
        attr_InstalledLanguage: "en_US".to_string(),
        attr_FullGameReleaseDate: release_date.unwrap_or_else(|| UNKNOWN_RELEASE_DATE.to_string()),
        attr_SystemTime: "2023-06-22T04:00:00".to_string()
    })
}
