use std::collections::HashMap;

use log::{debug, info, warn};

use crate::core::service_layer::{
    ServiceFriends, ServiceGetMyFriendsRequestBuilder, ServiceLayerError,
    SERVICE_REQUEST_GETMYFRIENDS,
};
use crate::{
    core::launch::EntitlementSource,
    lsx::{
        connection::LockedConnectionState,
        request::LSXRequestError,
        types::{
            LSXBlockedUser, LSXErrorSuccess, LSXFriend, LSXFriendState, LSXGetBlockList,
            LSXGetBlockListResponse, LSXGetPresence, LSXGetPresenceResponse, LSXGetProfile,
            LSXGetProfileResponse, LSXImage, LSXPresence, LSXQueryFriends, LSXQueryFriendsResponse,
            LSXQueryImage, LSXQueryImageResponse, LSXQueryPresence, LSXQueryPresenceResponse,
            LSXResponseType, LSXSetPresence,
        },
    },
    make_lsx_handler_response,
    presence::{BasicPresence, PresenceUpdate, RichPresence},
    util::native::{platform_path, NativeError, SafeStr},
};

pub async fn handle_profile_request(
    state: LockedConnectionState,
    _: LSXGetProfile,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    let arc = state.write().await.maxima_arc();
    let maxima = arc.lock().await;

    let user = maxima.local_user().await?;
    let path = platform_path(maxima.avatar_image(&user.id(), 208, 208).await?);

    let player = user
        .player()
        .as_ref()
        .ok_or(ServiceLayerError::MissingField)?;
    let name = player.unique_name();
    debug!("Got profile for {} {:?}", &name, path);

    // `IsSteamSubscriber` mirrors the `EntitlementSource` reported by
    // GetAllGameInfo (both from `ActiveGameContext::entitlement_source`) so
    // the two never contradict each other. `IsSubscriber` is an EA Play
    // subscription, which a launch can't tell us anything about.
    let is_steam_entitlement = maxima
        .playing()
        .as_ref()
        .is_some_and(|p| p.entitlement_source() == EntitlementSource::Steam);

    make_lsx_handler_response!(Response, GetProfileResponse, {
       attr_Persona: name.to_owned(),
       attr_SubscriberLevel: 0,
       attr_CommerceCurrency: "USD".to_string(),
       attr_IsTrialSubscriber: false,
       attr_Country: "US".to_string(),
       attr_UserId: user.id().parse::<u64>()?,
       attr_GeoCountry: "US".to_string(),
       attr_AvatarId: path.safe_str()?.to_string(),
       attr_IsSubscriber: false,
       attr_IsSteamSubscriber: is_steam_entitlement,
       attr_PersonaId: player.psd().parse::<u64>()?,
       attr_IsUnderAge: false,
       attr_UserIndex: 0,
    })
}

pub async fn handle_presence_request(
    _: LockedConnectionState,
    _: LSXGetPresence,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    make_lsx_handler_response!(Response, GetPresenceResponse, {
       attr_UserId: 1005663144213,
       attr_Presence: LSXPresence::Ingame,
       attr_Title: None,
       attr_TitleId: None,
       attr_MultiplayerId: None,
       attr_RichPresence: None,
       attr_GamePresence: None,
       attr_SessionId: None,
       attr_Group: None,
       attr_GroupId: None,
    })
}

pub async fn handle_set_presence_request(
    state: LockedConnectionState,
    request: LSXSetPresence,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    info!(
        "Setting Presence to {:?}: {}",
        request.attr_Presence,
        request
            .attr_RichPresence
            .to_owned()
            .or(Some(String::new()))
            .unwrap()
    );

    let arc = state.write().await.maxima_arc();
    let mut maxima = arc.lock().await;

    // When LSX connects from an externally-launched game (e.g. Steam Northstar
    // mode), maxima.playing() is None because the game wasn't started through
    // maxima-cli. Return a harmless success response so the connection stays
    // alive rather than panicking. (Upstream PR #42 / catornot patch-external-lsx)
    let Some(playing) = maxima.playing().as_ref() else {
        return make_lsx_handler_response!(Response, ErrorSuccess, { attr_Code: 0, attr_Description: String::new() });
    };
    if playing.mode().is_online_offline() {
        return make_lsx_handler_response!(Response, ErrorSuccess, { attr_Code: 0, attr_Description: String::new() });
    }

    let Some(owned_offer) = playing.offer().as_ref() else {
        return make_lsx_handler_response!(Response, ErrorSuccess, { attr_Code: 0, attr_Description: String::new() });
    };
    let offer = owned_offer.offer();
    let offer_id = offer.offer_id().to_owned();
    let name = offer.display_name().to_owned();

    if let Some(presence) = request.attr_RichPresence {
        let update = PresenceUpdate {
            basic: BasicPresence::Online,
            offer_id,
            game_title: name,
            rich_presence: presence,
            game_presence: request.attr_GamePresence,
            session_id: request.attr_SessionId,
            joinable: matches!(
                request.attr_Presence,
                LSXPresence::Joinable | LSXPresence::JoinableInviteOnly
            ),
            joinable_invite_only: request.attr_Presence == LSXPresence::JoinableInviteOnly,
        };

        // Presence is cosmetic: don't hand the game an error (and risk it
        // tearing down the LSX session) when the presence backend is down.
        if let Err(err) = maxima.rtm().update_presence(&update).await {
            warn!("Failed to update presence: {}", err);
        }
    }

    make_lsx_handler_response!(Response, ErrorSuccess, { attr_Code: 0, attr_Description: String::new() })
}

/// What the game is told about a friend's presence.
fn lsx_presence_of(presence: &RichPresence) -> LSXPresence {
    if presence.game().as_deref().is_some_and(|game| !game.is_empty()) {
        if *presence.joinable_invite_only() {
            LSXPresence::JoinableInviteOnly
        } else if *presence.joinable() {
            LSXPresence::Joinable
        } else {
            LSXPresence::Ingame
        }
    } else {
        match presence.basic() {
            BasicPresence::Unknown => LSXPresence::Unknown,
            BasicPresence::Offline => LSXPresence::Offline,
            BasicPresence::Dnd => LSXPresence::Busy,
            BasicPresence::Away => LSXPresence::Idle,
            BasicPresence::Online => LSXPresence::Online,
        }
    }
}

fn lsx_friend(
    user_id: u64,
    persona: String,
    persona_id: String,
    avatar_id: String,
    state: LSXFriendState,
    presence: &RichPresence,
) -> LSXFriend {
    LSXFriend {
        attr_TitleId: "".to_string(),
        attr_MultiplayerId: presence.multiplayer_id().clone().unwrap_or_default(),
        attr_Persona: persona,
        attr_RichPresence: presence.status().to_string(),
        attr_GamePresence: presence
            .game_presence()
            .clone()
            .or_else(|| presence.game().clone())
            .unwrap_or_default(),
        attr_Title: "".to_string(),
        attr_UserId: user_id,
        attr_PersonaId: persona_id,
        attr_AvatarId: avatar_id,
        attr_Group: presence.group_name().clone().unwrap_or_default(),
        attr_GroupId: presence.group_id().clone().unwrap_or_default(),
        attr_Presence: lsx_presence_of(presence),
        attr_State: state,
    }
}

/// Entries for the users the game asked about. A user we have no presence for
/// is simply left out: a game asking about someone who isn't a friend (or
/// hasn't been seen yet) gets a successful, shorter answer, never an error.
fn query_presence_entries(
    users: &[u64],
    lookup: impl Fn(&str) -> Option<RichPresence>,
    personas: &HashMap<String, String>,
) -> Vec<LSXFriend> {
    users
        .iter()
        .filter_map(|user| {
            let key = user.to_string();
            let presence = lookup(&key)?;
            let persona = personas
                .get(&key)
                .cloned()
                .unwrap_or_else(|| "------".to_string());
            Some(lsx_friend(
                *user,
                persona,
                "0".to_string(),
                "".to_string(),
                LSXFriendState::None,
                &presence,
            ))
        })
        .collect()
}

pub async fn handle_query_presence_request(
    state: LockedConnectionState,
    request: LSXQueryPresence,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    let mut state = state.write().await;
    let mut maxima = state.maxima().await;

    // Names are a nicety; never fail the query over them.
    let personas: HashMap<String, String> = match maxima.friends(0).await {
        Ok(friends) => friends
            .iter()
            .map(|f| (f.id().to_owned(), f.unique_name().to_string()))
            .collect(),
        Err(err) => {
            debug!("Friends unavailable for QueryPresence: {}", err);
            HashMap::new()
        }
    };

    let presence_store = maxima.rtm().presence_store().lock().await;
    let friends = query_presence_entries(&request.Users, |id| presence_store.get(id), &personas);

    make_lsx_handler_response!(Response, QueryPresenceResponse, { friend: friends })
}

pub async fn handle_query_friends_request(
    state: LockedConnectionState,
    _: LSXQueryFriends,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    let mut state = state.write().await;
    let mut maxima = state.maxima().await;

    let friends = maxima.friends(0).await?;
    let presence_store = maxima.rtm().presence_store().lock().await;

    let mut lsx_friends = Vec::new();
    for ele in friends {
        if ele.relationship() != "FRIEND" {
            continue;
        }

        let presence = presence_store
            .get(ele.id())
            .unwrap_or_else(RichPresence::offline);

        lsx_friends.push(lsx_friend(
            ele.id().parse()?,
            ele.unique_name().to_string(),
            ele.pd().parse()?,
            format!("user:{}", ele.id()),
            LSXFriendState::Mutual,
            &presence,
        ));
    }

    make_lsx_handler_response!(Response, QueryFriendsResponse, { friend: lsx_friends })
}

pub async fn handle_get_block_list_request(
    state: LockedConnectionState,
    _: LSXGetBlockList,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    let mut list: Vec<LSXBlockedUser> = Vec::new();

    let mut maxima = state.write().await;
    let maxima = maxima.maxima().await;
    let friends: ServiceFriends = maxima
        .service_layer()
        .request(
            SERVICE_REQUEST_GETMYFRIENDS,
            ServiceGetMyFriendsRequestBuilder::default()
                .limit(100)
                .offset(0)
                .is_mutual_friends_enabled(false)
                .build()
                .unwrap(),
        )
        .await?;

    for blocked in friends.blocked_players().items() {
        list.push(LSXBlockedUser {
            attr_UserId: blocked.pd().to_string(),
            attr_EAID: "".to_string(),
            attr_PersonaId: if let Some(player) = blocked.player_v2() {
                player.psd().to_string()
            } else {
                String::new()
            },
        });
    }

    make_lsx_handler_response!(Response, GetBlockListResponse, { attr_Return: "Success".to_string(), User: list})
}

pub async fn handle_query_image_request(
    state: LockedConnectionState,
    request: LSXQueryImage,
) -> Result<Option<LSXResponseType>, LSXRequestError> {
    let parts = request.attr_ImageId.split(":").collect::<Vec<_>>();

    let arc = state.write().await.maxima_arc();
    let maxima = arc.lock().await;

    let path = platform_path(
        maxima
            .avatar_image(parts[1], request.attr_Width, request.attr_Height)
            .await?,
    );

    let mut images = Vec::new();
    images.push(LSXImage {
        attr_ImageId: request.attr_ImageId,
        attr_Width: request.attr_Width,
        attr_Height: request.attr_Height,
        attr_ResourcePath: path.safe_str()?.to_string(),
    });

    make_lsx_handler_response!(Response, QueryImageResponse, { attr_Result: 1, image: images, })
}

#[cfg(test)]
mod presence_tests {
    use super::*;

    fn in_game() -> RichPresence {
        RichPresence::new(
            BasicPresence::Online,
            "Titanfall 2: In the menus".into(),
            Some("Origin.OFR.50.0001456".into()),
        )
    }

    #[test]
    fn unknown_users_get_an_empty_successful_answer() {
        let personas = HashMap::new();
        let entries = query_presence_entries(&[42, 43], |_| None, &personas);
        assert!(entries.is_empty());
    }

    #[test]
    fn known_users_are_answered_and_unknown_ones_skipped() {
        let mut personas = HashMap::new();
        personas.insert("1".to_string(), "pilot".to_string());
        let entries = query_presence_entries(
            &[1, 2],
            |id| (id == "1").then(in_game),
            &personas,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].attr_UserId, 1);
        assert_eq!(entries[0].attr_Persona, "pilot");
        assert_eq!(entries[0].attr_Presence, LSXPresence::Ingame);
        assert_eq!(entries[0].attr_RichPresence, "Titanfall 2: In the menus");
        assert_eq!(entries[0].attr_GamePresence, "Origin.OFR.50.0001456");
    }

    #[test]
    fn persona_falls_back_when_the_friends_list_is_unavailable() {
        let entries = query_presence_entries(&[7], |_| Some(in_game()), &HashMap::new());
        assert_eq!(entries[0].attr_Persona, "------");
    }

    #[test]
    fn presence_mapping_covers_joinable_and_basic_states() {
        assert_eq!(
            lsx_presence_of(&in_game().with_joinable(true, false)),
            LSXPresence::Joinable
        );
        assert_eq!(
            lsx_presence_of(&in_game().with_joinable(true, true)),
            LSXPresence::JoinableInviteOnly
        );
        assert_eq!(
            lsx_presence_of(&RichPresence::new(BasicPresence::Away, String::new(), None)),
            LSXPresence::Idle
        );
        assert_eq!(lsx_presence_of(&RichPresence::offline()), LSXPresence::Offline);
    }

    #[test]
    fn rich_backend_fields_reach_the_game() {
        let presence = in_game()
            .with_multiplayer_id(Some("1039093".into()))
            .with_game_presence(Some("blob".into()))
            .with_group(Some("g-1".into()), Some("squad".into()));
        let friend = lsx_friend(
            9,
            "p".into(),
            "0".into(),
            "".into(),
            LSXFriendState::Mutual,
            &presence,
        );
        assert_eq!(friend.attr_MultiplayerId, "1039093");
        assert_eq!(friend.attr_GamePresence, "blob");
        assert_eq!(friend.attr_Group, "squad");
        assert_eq!(friend.attr_GroupId, "g-1");
    }
}
