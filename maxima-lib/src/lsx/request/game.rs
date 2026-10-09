const LANGUAGES: &str =
    "ar_SA,de_DE,en_US,es_ES,es_MX,fr_FR,it_IT,ja_JP,ko_KR,pl_PL,pt_BR,ru_RU,zh_CN,zh_TW";
//const LANGUAGES: &str = "de_DE,en_US,es_ES,es_MX,fr_FR,it_IT,ja_JP,pl_PL,pt_BR,ru_RU,zh_TW";
//const LANGUAGES: &str = "en_US,es_ES,fr_FR,pt_BR";

use crate::{
    core::launch::EntitlementSource,
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

async fn installed_language(state: &LockedConnectionState) -> String {
    let arc = state.write().await.maxima_arc();
    let maxima = arc.lock().await;
    maxima.locale().full_str().to_string()
}

pub async fn handle_game_info_request(
    state: LockedConnectionState,
    request: LSXGetGameInfo,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    let game_info = match request.attr_GameInfoId {
        LSXGameInfoId::FreeTrial => "false".to_string(),
        LSXGameInfoId::Languages => LANGUAGES.to_string(),
        LSXGameInfoId::InstalledLanguage => installed_language(&state).await,
    };

    make_lsx_handler_response!(Response, GetGameInfoResponse, { attr_GameInfo: game_info })
}

// Sample EA Desktop response (from a real LSX trace) for
// reference of the field shape — DO NOT use these values, they're stale:
// <GetAllGameInfoResponse FullGamePurchased="true" FullGameReleased="true"
//   InstalledVersion="0" MaxGroupSize="16" Languages="..."
//   Expiration="0000-00-00T00:00:00" UpToDate="true" HasExpiration="false"
//   InstalledLanguage="" EntitlementSource="STEAM"
//   FullGameReleaseDate="2020-10-22T09:00:00" AvailableVersion="1.0.64.43203"
//   DisplayName="Example Game Definitive Edition" FreeTrial="false"
//   SystemTime="2023-06-23T04:22:10"/>

const NEUTRAL_DATE: &str = "0000-00-00T00:00:00";

/// Handles `GetAllGameInfo` — the LSX request the game uses to verify that
/// the auth server's view of "what's installed" matches what's on disk.
///
/// Everything here is about the game that is actually running: the display
/// name and versions are what the client reported in the LSX challenge
/// (`Version` / `Title`, captured via `set_game_metadata`), else the active
/// offer's library data, else empty. Nothing in this handler is specific to
/// one title.
///
/// `InstalledVersion` and `AvailableVersion` need to agree with the client's
/// own idea of its version: some games treat a mismatch as tampering.
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
    // IsSteamSubscriber=false) can be read as a tamper signal by a game's
    // DRM stub. Both come from `ActiveGameContext::entitlement_source`.
    let (offer, entitlement_source, installed_language) = {
        let arc = state.write().await.maxima_arc();
        let maxima = arc.lock().await;
        let playing = maxima.playing().as_ref();
        (
            playing.and_then(|p| p.offer().clone()),
            playing.map_or(EntitlementSource::Ea, |p| p.entitlement_source()),
            maxima.locale().full_str().to_string(),
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

    // The values the game reported about itself in the challenge win: they
    // are what it expects echoed back, and echoing them is the behaviour
    // validated end-to-end. Library data only fills in what the game omitted.
    let display_name = challenge_title.or(display_name).unwrap_or_default();
    let installed_version = challenge_version
        .clone()
        .or(installed_version)
        .unwrap_or_default();
    let available_version = challenge_version
        .or(available_version)
        .unwrap_or_default();

    make_lsx_handler_response!(Response, GetAllGameInfoResponse, {
        attr_FullGamePurchased: true,
        attr_FullGameReleased: true,
        attr_InstalledVersion: installed_version,
        attr_MaxGroupSize: 16,
        attr_Languages: LANGUAGES.to_string(),
        attr_Expiration: NEUTRAL_DATE.to_string(),
        attr_UpToDate: true,
        attr_HasExpiration: false,
        attr_EntitlementSource: entitlement_source.lsx_tag().to_string(),
        attr_AvailableVersion: available_version,
        attr_DisplayName: display_name,
        attr_FreeTrial: false,
        attr_InstalledLanguage: installed_language,
        attr_FullGameReleaseDate: release_date.unwrap_or_else(|| NEUTRAL_DATE.to_string()),
        attr_SystemTime: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()
    })
}
