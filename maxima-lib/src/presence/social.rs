//! The wire format of EA's social presence service (upstream PR 67) and the
//! mapping between it and [`RichPresence`] / [`PresenceUpdate`].
//!
//! Pure data, no transport: compiled in every build so the mapping can be
//! tested offline. The `presence-grpc` feature adds the network side
//! ([`super::grpc`]).

use log::debug;

use self::proto::eadp::{
    common::v2::PlayerId,
    social::presence::v1::{
        presence_update::PropertiesEnt, value::Value as ValueKind, PresenceNotification,
        PresenceUpdate as ProtoPresenceUpdate, Value,
    },
};
use super::model::{BasicPresence, PresenceUpdate, RichPresence};

#[allow(clippy::all)]
pub mod proto {
    pub mod eadp {
        pub mod common {
            pub mod v1 {
                include!(concat!(env!("OUT_DIR"), "/eadp.common.v1.rs"));

                impl PlayerNetworkId {
                    pub fn ea() -> Self {
                        Self { id: "EA".to_owned() }
                    }
                }

                impl ProductId {
                    /// EA app's product id for the Juno client.
                    pub fn juno() -> Self {
                        Self {
                            id: "01eb04f5-ad3f-7a1c-ff56-892bb262b1a4".to_owned(),
                        }
                    }
                }

                impl DevicePlatformId {
                    pub fn pc() -> Self {
                        Self { id: "PC".to_owned() }
                    }
                }
            }
            pub mod v2 {
                include!(concat!(env!("OUT_DIR"), "/eadp.common.v2.rs"));
            }
        }
        pub mod social {
            pub mod presence {
                pub mod v1 {
                    include!(concat!(env!("OUT_DIR"), "/eadp.social.presence.v1.rs"));
                }
            }
        }
    }
}

pub(crate) const KEY_PRODUCT_ID: &str = "ea_app.productId";
pub(crate) const KEY_GAME_TITLE: &str = "ea_app.gameTitle";
pub(crate) const KEY_RICH_PRESENCE: &str = "ea_app.richPresence";
pub(crate) const KEY_PRESENCE_STATUS: &str = "ea_app.presenceStatus";
pub(crate) const KEY_AVAILABILITY: &str = "ea_app.presenceAvailability";
pub(crate) const KEY_INVISIBLE: &str = "ea_app.presenceIsInvisible";
pub(crate) const KEY_JOINABLE: &str = "ea_app.isJoinable";
pub(crate) const KEY_JOINABLE_INVITE_ONLY: &str = "ea_app.isJoinableInviteOnly";
pub(crate) const KEY_MULTIPLAYER_ID: &str = "ea_app.multiplayerId";
pub(crate) const KEY_GROUP_ID: &str = "ea_app.groupGuid";
pub(crate) const KEY_GROUP_NAME: &str = "ea_app.groupName";

pub(crate) const AVAILABILITY_ONLINE: i64 = -1;
pub(crate) const AVAILABILITY_AWAY: i64 = -2;

impl PropertiesEnt {
    fn string(key: &str, value: impl Into<String>) -> Self {
        Self {
            key: key.to_owned(),
            value: Some(Value {
                value: Some(ValueKind::StringValue(value.into())),
            }),
        }
    }

    fn integer(key: &str, value: i64) -> Self {
        Self {
            key: key.to_owned(),
            value: Some(Value {
                value: Some(ValueKind::IntegerValue(value)),
            }),
        }
    }
}

/// The properties EA app sets for the local user's presence.
pub(crate) fn update_properties(update: &PresenceUpdate) -> Vec<PropertiesEnt> {
    let mut props = Vec::new();

    if !update.offer_id.is_empty() {
        props.push(PropertiesEnt::string(KEY_PRODUCT_ID, update.offer_id.clone()));
    }
    if !update.game_title.is_empty() {
        props.push(PropertiesEnt::string(KEY_GAME_TITLE, update.game_title.clone()));
    }
    if !update.rich_presence.is_empty() {
        props.push(PropertiesEnt::string(
            KEY_RICH_PRESENCE,
            update.rich_presence.clone(),
        ));
    }

    // EA app's own status line is the title followed by the rich presence.
    let status = [update.game_title.as_str(), update.rich_presence.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !status.is_empty() {
        props.push(PropertiesEnt::string(KEY_PRESENCE_STATUS, status));
    }

    props.push(PropertiesEnt::integer(KEY_JOINABLE, update.joinable.into()));
    props.push(PropertiesEnt::integer(
        KEY_JOINABLE_INVITE_ONLY,
        update.joinable_invite_only.into(),
    ));
    if let Some(session_id) = update.session_id.as_ref().filter(|id| !id.is_empty()) {
        props.push(PropertiesEnt::string(KEY_GROUP_ID, session_id.clone()));
    }

    props.push(PropertiesEnt::integer(KEY_INVISIBLE, 0));
    props.push(PropertiesEnt::integer(
        KEY_AVAILABILITY,
        match update.basic {
            BasicPresence::Online => AVAILABILITY_ONLINE,
            BasicPresence::Away => AVAILABILITY_AWAY,
            _ => 0,
        },
    ));

    props
}

pub(crate) fn proto_update(update: &PresenceUpdate) -> ProtoPresenceUpdate {
    ProtoPresenceUpdate {
        appear_offline: update.basic == BasicPresence::Offline,
        properties: update_properties(update),
    }
}

/// A friend's presence from one notification. `None` when the notification
/// doesn't say who it is about.
pub(crate) fn presence_from_notification(
    notification: &PresenceNotification,
) -> Option<(String, RichPresence)> {
    let PlayerId { id } = notification.player_id.clone()?;
    if id.is_empty() {
        return None;
    }

    let Some(session) = notification
        .session_notification
        .as_ref()
        .filter(|_| notification.player_online)
    else {
        return Some((id, RichPresence::offline()));
    };

    let mut basic = BasicPresence::Online;
    let mut offer_id = None;
    let mut game_title = None;
    let mut rich = None;
    let mut status = None;
    let mut multiplayer_id = None;
    let mut group_id = None;
    let mut group_name = None;
    let mut joinable = false;
    let mut joinable_invite_only = false;

    for prop in &session.properties {
        let Some(value) = prop.value.as_ref().and_then(|v| v.value.as_ref()) else {
            continue;
        };
        match (prop.key.as_str(), value) {
            (KEY_AVAILABILITY, ValueKind::IntegerValue(v)) => {
                basic = if *v == AVAILABILITY_AWAY {
                    BasicPresence::Away
                } else {
                    BasicPresence::Online
                };
            }
            (KEY_PRODUCT_ID, ValueKind::StringValue(v)) if !v.is_empty() => {
                offer_id = Some(v.clone())
            }
            (KEY_GAME_TITLE, ValueKind::StringValue(v)) if !v.is_empty() => {
                game_title = Some(v.clone())
            }
            (KEY_RICH_PRESENCE, ValueKind::StringValue(v)) => rich = Some(v.clone()),
            (KEY_PRESENCE_STATUS, ValueKind::StringValue(v)) => status = Some(v.clone()),
            (KEY_MULTIPLAYER_ID, ValueKind::StringValue(v)) if !v.is_empty() => {
                multiplayer_id = Some(v.clone())
            }
            (KEY_GROUP_ID, ValueKind::StringValue(v)) if !v.is_empty() => {
                group_id = Some(v.clone())
            }
            (KEY_GROUP_NAME, ValueKind::StringValue(v)) if !v.is_empty() => {
                group_name = Some(v.clone())
            }
            (KEY_JOINABLE, ValueKind::IntegerValue(v)) => joinable = *v > 0,
            (KEY_JOINABLE_INVITE_ONLY, ValueKind::IntegerValue(v)) => {
                joinable_invite_only = *v > 0
            }
            (key, _) => debug!("Ignoring presence property `{}`", key),
        }
    }

    let status = status
        .filter(|s| !s.is_empty())
        .or(rich.filter(|s| !s.is_empty()))
        .unwrap_or_else(|| session.activity_description.clone());

    Some((
        id,
        RichPresence::new(basic, status, offer_id)
            .with_game_title(game_title)
            .with_multiplayer_id(multiplayer_id)
            .with_group(group_id, group_name)
            .with_joinable(joinable, joinable_invite_only),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::proto::eadp::social::presence::v1::{
        presence_session_notification, ConnectToPresenceSessionResponse,
        PresenceSessionNotification,
    };
    use prost::Message;

    fn prop_str(key: &str, value: &str) -> presence_session_notification::PropertiesEnt {
        presence_session_notification::PropertiesEnt {
            key: key.to_owned(),
            value: Some(Value {
                value: Some(ValueKind::StringValue(value.to_owned())),
            }),
        }
    }

    fn prop_int(key: &str, value: i64) -> presence_session_notification::PropertiesEnt {
        presence_session_notification::PropertiesEnt {
            key: key.to_owned(),
            value: Some(Value {
                value: Some(ValueKind::IntegerValue(value)),
            }),
        }
    }

    fn notification(
        online: bool,
        props: Vec<presence_session_notification::PropertiesEnt>,
    ) -> PresenceNotification {
        PresenceNotification {
            player_id: Some(PlayerId { id: "1000".into() }),
            player_online: online,
            session_notification: Some(PresenceSessionNotification {
                properties: props,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn friend_in_game_maps_to_a_rich_presence() {
        let n = notification(
            true,
            vec![
                prop_int(KEY_AVAILABILITY, AVAILABILITY_ONLINE),
                prop_str(KEY_PRODUCT_ID, "Origin.OFR.50.0001456"),
                prop_str(KEY_GAME_TITLE, "Titanfall 2"),
                prop_str(KEY_RICH_PRESENCE, "In the menus"),
                prop_str(KEY_PRESENCE_STATUS, "Titanfall 2 In the menus"),
                prop_str(KEY_MULTIPLAYER_ID, "1039093"),
                prop_int(KEY_JOINABLE, 1),
                prop_str("ea_app.somethingNew", "ignored"),
            ],
        );
        let (id, presence) = presence_from_notification(&n).unwrap();
        assert_eq!(id, "1000");
        assert_eq!(*presence.basic(), BasicPresence::Online);
        assert_eq!(presence.status(), "Titanfall 2 In the menus");
        assert_eq!(presence.game().as_deref(), Some("Origin.OFR.50.0001456"));
        assert_eq!(presence.game_title().as_deref(), Some("Titanfall 2"));
        assert_eq!(presence.multiplayer_id().as_deref(), Some("1039093"));
        assert!(*presence.joinable());
        assert!(!*presence.joinable_invite_only());
    }

    #[test]
    fn away_offline_and_unspecified_availability() {
        let away = notification(true, vec![prop_int(KEY_AVAILABILITY, AVAILABILITY_AWAY)]);
        assert_eq!(
            *presence_from_notification(&away).unwrap().1.basic(),
            BasicPresence::Away
        );

        let unspecified = notification(true, vec![]);
        assert_eq!(
            *presence_from_notification(&unspecified).unwrap().1.basic(),
            BasicPresence::Online
        );

        let offline = notification(false, vec![prop_str(KEY_GAME_TITLE, "stale")]);
        assert_eq!(
            presence_from_notification(&offline).unwrap().1,
            RichPresence::offline()
        );
    }

    #[test]
    fn rich_presence_is_the_fallback_status() {
        let n = notification(true, vec![prop_str(KEY_RICH_PRESENCE, "Lobby")]);
        assert_eq!(presence_from_notification(&n).unwrap().1.status(), "Lobby");
    }

    #[test]
    fn malformed_notifications_are_skipped_not_fatal() {
        let mut anonymous = notification(true, vec![]);
        anonymous.player_id = None;
        assert!(presence_from_notification(&anonymous).is_none());

        let mut empty_id = notification(true, vec![]);
        empty_id.player_id = Some(PlayerId { id: String::new() });
        assert!(presence_from_notification(&empty_id).is_none());

        // Wrong value types and value-less properties are ignored.
        let odd = notification(
            true,
            vec![
                prop_int(KEY_PRODUCT_ID, 5),
                presence_session_notification::PropertiesEnt {
                    key: KEY_GAME_TITLE.to_owned(),
                    value: None,
                },
            ],
        );
        let (_, presence) = presence_from_notification(&odd).unwrap();
        assert_eq!(presence.game(), &None);
        assert_eq!(presence.game_title(), &None);
    }

    #[test]
    fn local_update_uses_ea_app_property_keys() {
        let update = PresenceUpdate {
            basic: BasicPresence::Online,
            offer_id: "Origin.OFR.50.0001456".into(),
            game_title: "Titanfall 2".into(),
            rich_presence: "In the menus".into(),
            session_id: Some("g-1".into()),
            joinable: true,
            ..Default::default()
        };
        let proto = proto_update(&update);
        assert!(!proto.appear_offline);

        let get = |key: &str| {
            proto
                .properties
                .iter()
                .find(|p| p.key == key)
                .and_then(|p| p.value.clone())
                .and_then(|v| v.value)
        };
        assert_eq!(
            get(KEY_PRODUCT_ID),
            Some(ValueKind::StringValue("Origin.OFR.50.0001456".into()))
        );
        assert_eq!(
            get(KEY_PRESENCE_STATUS),
            Some(ValueKind::StringValue("Titanfall 2 In the menus".into()))
        );
        assert_eq!(get(KEY_JOINABLE), Some(ValueKind::IntegerValue(1)));
        assert_eq!(
            get(KEY_AVAILABILITY),
            Some(ValueKind::IntegerValue(AVAILABILITY_ONLINE))
        );
        assert_eq!(get(KEY_GROUP_ID), Some(ValueKind::StringValue("g-1".into())));
    }

    #[test]
    fn empty_update_only_carries_the_flags() {
        let proto = proto_update(&PresenceUpdate::default());
        let keys: Vec<_> = proto.properties.iter().map(|p| p.key.as_str()).collect();
        assert!(!keys.contains(&KEY_PRODUCT_ID));
        assert!(!keys.contains(&KEY_PRESENCE_STATUS));
        assert!(keys.contains(&KEY_AVAILABILITY));
    }

    #[test]
    fn notification_survives_an_encode_decode_round_trip() {
        let original = ConnectToPresenceSessionResponse {
            presence_notification: Some(notification(
                true,
                vec![prop_str(KEY_GAME_TITLE, "Titanfall 2")],
            )),
        };
        let bytes = original.encode_to_vec();
        let decoded = ConnectToPresenceSessionResponse::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded, original);
        let (_, presence) =
            presence_from_notification(&decoded.presence_notification.unwrap()).unwrap();
        assert_eq!(presence.game_title().as_deref(), Some("Titanfall 2"));
    }

    #[test]
    fn update_request_round_trips() {
        use proto::eadp::social::presence::v1::{
            PartialUpdatePresenceSessionRequest, PresenceSessionToken,
        };
        let request = PartialUpdatePresenceSessionRequest {
            presence_session_token: Some(PresenceSessionToken {
                token: vec![1, 2, 3],
            }),
            presence_update: Some(proto_update(&PresenceUpdate {
                offer_id: "Origin.OFR.50.0001456".into(),
                ..Default::default()
            })),
        };
        let decoded =
            PartialUpdatePresenceSessionRequest::decode(request.encode_to_vec().as_slice())
                .unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn unknown_fields_such_as_timestamps_are_skipped_on_decode() {
        // PresenceNotification field 3 (lastSeenOnline, a Timestamp we don't
        // model) must not break decoding of the fields we do read.
        let mut bytes = notification(true, vec![prop_str(KEY_GAME_TITLE, "TF2")]).encode_to_vec();
        bytes.extend_from_slice(&[0x1a, 0x02, 0x08, 0x01]);
        let decoded = PresenceNotification::decode(bytes.as_slice()).unwrap();
        assert!(presence_from_notification(&decoded).is_some());
    }

    #[test]
    fn garbage_bytes_are_a_decode_error_not_a_panic() {
        assert!(PresenceNotification::decode(&[0xff, 0xff, 0xff][..]).is_err());
    }
}
