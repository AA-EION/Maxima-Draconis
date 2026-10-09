use core::future::Future;
use async_trait::async_trait;
use derive_getters::Getters;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::{
    connection::RtmConnectionManager,
    proto::{
        communication_v1, success_v1, BasicPresenceType, HeartbeatV1, LoginV3Response, Player,
        PresenceSubscribeAllFriendsV1, PresenceUpdateV1, RichPresenceType, RichPresenceV1,
        SessionCleanupV1,
    },
    RtmError,
};
use crate::{
    core::auth::storage::{AuthError, LockedAuthStorage},
    presence::{
        backend::{PresenceBackend, PresenceBackendKind},
        model::{LockedPresenceStore, PresenceSink, PresenceUpdate},
    },
    rtm::proto::{LoginRequestV3, PlatformV1, PresenceSubscribeV1, PresenceV1, UserType},
};

pub use crate::presence::model::{BasicPresence, RichPresence, RichPresenceBuilder};

macro_rules! send_and_forget_rtm_request {
    ($connection_manager: expr, $request_body_name: ident, $comm_name: ident, $comm_initializer:tt) => {
        $connection_manager.send_and_forget_request(communication_v1::Body::$request_body_name($comm_name $comm_initializer))
    }
}

macro_rules! send_rtm_request {
    ($connection_manager: expr, $request_body_name: ident, $comm_name: ident, $response_body_name: ident, $response_comm_name: ident, $comm_initializer:tt) => {
        {
            fn _rtm_transform(
                fut: impl Future<Output = Result<communication_v1::Body, RtmError>> + Send,
            ) -> impl Future<Output = Result<$response_comm_name, RtmError>> + Send {
                async move {
                    match fut.await? {
                        communication_v1::Body::Success(success) => match success.body {
                            Some(body) => match body {
                                success_v1::Body::$response_body_name(data) => Ok(data),
                                any => Err(RtmError::InvalidResponse(any)),
                            },
                            None => Err(RtmError::NoBody),
                        }
                        communication_v1::Body::Error(err) => Err(RtmError::V1(err)),
                        any => Err(RtmError::InvalidVariant(any)),
                    }
                }
            }

            _rtm_transform($connection_manager.send_request(communication_v1::Body::$request_body_name($comm_name $comm_initializer)))
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClientVersion {
    client_type: String,
    version: String,
    integrations: String,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CustomRichPresenceData {
    game_product_id: String,
    version: i32,
}

impl RichPresence {
    pub fn from(presence: &PresenceV1) -> Self {
        let basic = match presence.basic_presence_type() {
            BasicPresenceType::Offline => BasicPresence::Offline,
            BasicPresenceType::Dnd => BasicPresence::Dnd,
            BasicPresenceType::Away => BasicPresence::Away,
            BasicPresenceType::Online => BasicPresence::Online,
            _ => BasicPresence::Unknown,
        };

        let rich = presence.rich_presence.clone().unwrap_or_default();
        let custom_data: CustomRichPresenceData =
            serde_json::from_str(&rich.custom_rich_presence_data).unwrap_or_default();

        RichPresence::new(
            basic,
            rich.game,
            if !custom_data.game_product_id.is_empty() {
                Some(custom_data.game_product_id)
            } else {
                None
            },
        )
    }
}

pub enum RtmEvent {
    PresenceUpdate(RichPresence),
}

/// How the payloads we send are shaped. Both speak to the same
/// `rtm.tnt-ea.com:9000` service with the same framing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtmDialect {
    /// What this fork has always sent.
    Legacy,
    /// Upstream PR 70 ("Antelope"): availability JSON in `status`, the game
    /// name in the rich presence, and an all-friends subscription.
    Antelope,
}

/// The `status` field Antelope clients send: the availability as JSON.
pub(crate) fn antelope_status_json(basic: BasicPresenceType) -> String {
    serde_json::json!({ "presenceavailability": basic.as_str_name().to_lowercase() }).to_string()
}

fn basic_presence_type(basic: &BasicPresence) -> BasicPresenceType {
    match basic {
        BasicPresence::Unknown => BasicPresenceType::UnknownPresence,
        BasicPresence::Offline => BasicPresenceType::Offline,
        BasicPresence::Dnd => BasicPresenceType::Dnd,
        BasicPresence::Away => BasicPresenceType::Away,
        BasicPresence::Online => BasicPresenceType::Online,
    }
}

#[derive(Getters)]
pub struct RtmClient {
    #[getter(skip)]
    auth: LockedAuthStorage,

    conn_man: RtmConnectionManager,
    sink: PresenceSink,
    dialect: RtmDialect,
}

impl RtmClient {
    pub fn new(auth: LockedAuthStorage) -> RtmClient {
        RtmClient::with_sink(auth, RtmDialect::Legacy, PresenceSink::new())
    }

    pub fn with_sink(auth: LockedAuthStorage, dialect: RtmDialect, sink: PresenceSink) -> RtmClient {
        let (sender_tx, mut receiver_tx) = mpsc::channel(32);

        let client = Self {
            conn_man: RtmConnectionManager::new(std::time::Duration::from_millis(50), sender_tx),
            auth,
            sink,
            dialect,
        };

        let cloned_sink = client.sink.clone();
        tokio::spawn(async move {
            while let Some(body) = receiver_tx.recv().await {
                if let Err(err) = RtmClient::process_update(body, &cloned_sink).await {
                    error!("Failed to process update: {}", err);
                }
            }
        });

        client
    }

    pub fn presence_store(&self) -> &LockedPresenceStore {
        self.sink.store()
    }

    async fn process_update(
        body: communication_v1::Body,
        sink: &PresenceSink,
    ) -> Result<(), RtmError> {
        match body {
            communication_v1::Body::Presence(presence) => {
                if presence.client_version.is_none() {
                    return Ok(());
                }

                let res: ClientVersion = serde_json::from_str(
                    presence
                        .client_version
                        .as_ref()
                        .ok_or(RtmError::InvalidClientVersion)?,
                )?;

                if res.client_type != "Client" && res.client_type != "LegacyClient" {
                    return Ok(());
                }

                let rich = RichPresence::from(&presence);

                if let Some(player) = presence.player.as_ref() {
                    let id = player.player_id.to_owned();
                    sink.publish(id.to_owned(), rich).await;

                    debug!("Updated {}'s presence", id);
                } else {
                    error!("Could not update player's presence (no player ID)!")
                }

                Ok(())
            }
            communication_v1::Body::PresenceUpdateError(err) => {
                warn!("RTM rejected our presence update: {} ({})", err.error_message, err.error_code);
                Ok(())
            }
            communication_v1::Body::PresenceSubscriptionError(err) => {
                warn!("RTM rejected a presence subscription: {} ({})", err.error_message, err.error_code);
                Ok(())
            }
            communication_v1::Body::PresenceSubscriptionAllFriendsErrorV1(err) => {
                warn!("RTM rejected the all-friends subscription: {} ({})", err.error_message, err.error_code);
                Ok(())
            }
            _ => Err(RtmError::UnhandledUpdate(body)),
        }
    }

    pub async fn login(&mut self) -> Result<(), RtmError> {
        let token = self
            .auth
            .lock()
            .await
            .access_token()
            .await?
            .ok_or(AuthError::NoAuthCode)?
            .to_owned();

        let version = format!(
            "{}-{}-mxa",
            env!("CARGO_CRATE_NAME"),
            env!("CARGO_PKG_VERSION")
        );
        info!("Connecting to RTM with version {}", version);

        let client_version = ClientVersion {
            client_type: "Client".to_owned(),
            version,
            integrations: "".to_owned(),
        };

        let res = send_rtm_request!(self.conn_man, LoginRequestV3, LoginRequestV3, LoginV3Response, LoginV3Response, {
            token: token.to_owned(),
            reconnect: false,
            heartbeat: false,
            user_type: UserType::Nucleus as i32,
            product_id: "origin".to_owned(),
            platform: PlatformV1::Pc as i32,
            client_version: serde_json::to_string(&client_version)?,
            session_key: None,
            force_disconnect_session_key: None,
        }).await?;

        for ele in res.connected_sessions {
            let Ok(platform) = PlatformV1::try_from(ele.platform) else {
                warn!("Ignoring RTM session with unknown platform {}", ele.platform);
                continue;
            };
            if platform != PlatformV1::Pc {
                continue;
            }

            self.session_cleanup(&ele.session_key).await?;
        }

        info!("Successfully logged into RTM");
        Ok(())
    }

    pub async fn set_presence(
        &mut self,
        basic_presence: BasicPresence,
        status: &str,
        offer_id: &str,
    ) -> Result<(), RtmError> {
        self.send_presence_update(&PresenceUpdate {
            basic: basic_presence,
            offer_id: offer_id.to_owned(),
            rich_presence: status.to_owned(),
            ..Default::default()
        })
        .await
    }

    pub async fn send_presence_update(&mut self, update: &PresenceUpdate) -> Result<(), RtmError> {
        let message = build_presence_update(self.dialect, update)?;
        info!("Updating RTM presence to '{}'", update.legacy_status());

        self.conn_man
            .send_and_forget_request(communication_v1::Body::PresenceUpdate(message))
            .await
    }

    /// Subscribe to a list of user IDs' presences
    pub async fn subscribe(&mut self, players: &[String]) -> Result<(), RtmError> {
        send_and_forget_rtm_request!(self.conn_man, PresenceSubscribe, PresenceSubscribeV1, {
            players: players.iter().map(|id| Player{ player_id: id.to_owned(), product_id: String::from("origin"), }).collect()
        })
        .await
    }

    /// Subscribe to every friend's presence, server-side.
    pub async fn subscribe_all(&mut self) -> Result<(), RtmError> {
        send_and_forget_rtm_request!(
            self.conn_man,
            PresenceSubscribeAllFriendsV1,
            PresenceSubscribeAllFriendsV1,
            {}
        )
        .await
    }

    pub async fn session_cleanup(&mut self, session_key: &str) -> Result<(), RtmError> {
        send_and_forget_rtm_request!(self.conn_man, SessionCleanupV1, SessionCleanupV1, {
            session_key: session_key.to_owned()
        })
        .await
    }

    pub async fn heartbeat(&mut self) -> Result<(), RtmError> {
        send_and_forget_rtm_request!(self.conn_man, Heartbeat, HeartbeatV1, {}).await
    }
}

/// The outgoing presence message for a dialect.
pub(crate) fn build_presence_update(
    dialect: RtmDialect,
    update: &PresenceUpdate,
) -> Result<PresenceUpdateV1, RtmError> {
    let rpc_data = CustomRichPresenceData {
        game_product_id: update.offer_id.clone(),
        version: 1,
    };
    let basic = basic_presence_type(&update.basic);

    let (status, game) = match dialect {
        RtmDialect::Legacy => (String::new(), update.legacy_status()),
        RtmDialect::Antelope => (
            antelope_status_json(basic),
            if update.game_title.is_empty() {
                update.legacy_status()
            } else {
                update.game_title.clone()
            },
        ),
    };

    Ok(PresenceUpdateV1 {
        status,
        basic_presence_type: basic as i32,
        user_defined_presence: "".to_owned(),
        rich_presence: Some(RichPresenceV1 {
            game,
            platform: PlatformV1::Pc as i32,
            game_mode_type: "".to_owned(),
            game_mode: "".to_owned(),
            game_session_data: "".to_owned(),
            rich_presence_type: RichPresenceType::UnknownRichPresence as i32,
            start_timestamp: "".to_owned(),
            end_timestamp: "".to_owned(),
            custom_rich_presence_data: serde_json::to_string(&rpc_data)?,
        }),
    })
}

#[async_trait]
impl PresenceBackend for RtmClient {
    fn kind(&self) -> PresenceBackendKind {
        match self.dialect {
            RtmDialect::Legacy => PresenceBackendKind::Legacy,
            RtmDialect::Antelope => PresenceBackendKind::Antelope,
        }
    }

    fn sink(&self) -> &PresenceSink {
        &self.sink
    }

    async fn login(&mut self) -> Result<(), RtmError> {
        RtmClient::login(self).await
    }

    async fn subscribe(&mut self, friend_ids: &[String]) -> Result<(), RtmError> {
        if self.dialect == RtmDialect::Antelope {
            self.subscribe_all().await?;
        }
        RtmClient::subscribe(self, friend_ids).await
    }

    async fn set_presence(&mut self, update: &PresenceUpdate) -> Result<(), RtmError> {
        self.send_presence_update(update).await
    }

    async fn heartbeat(&mut self) -> Result<(), RtmError> {
        RtmClient::heartbeat(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn menu_update() -> PresenceUpdate {
        PresenceUpdate {
            basic: BasicPresence::Online,
            offer_id: "Origin.OFR.50.0001456".into(),
            game_title: "Titanfall 2".into(),
            rich_presence: "In the menus".into(),
            ..Default::default()
        }
    }

    #[test]
    fn legacy_dialect_is_the_historic_message() {
        let built = build_presence_update(RtmDialect::Legacy, &menu_update()).unwrap();
        assert_eq!(built.status, "");
        assert_eq!(built.basic_presence_type, BasicPresenceType::Online as i32);
        let rich = built.rich_presence.unwrap();
        assert_eq!(rich.game, "Titanfall 2: In the menus");
        assert_eq!(
            rich.custom_rich_presence_data,
            r#"{"gameProductId":"Origin.OFR.50.0001456","version":1}"#
        );
    }

    #[test]
    fn antelope_dialect_sends_availability_json_and_the_game_name() {
        let built = build_presence_update(RtmDialect::Antelope, &menu_update()).unwrap();
        assert_eq!(built.status, r#"{"presenceavailability":"online"}"#);
        assert_eq!(built.rich_presence.unwrap().game, "Titanfall 2");

        let bare = PresenceUpdate {
            basic: BasicPresence::Away,
            rich_presence: "Idle".into(),
            ..Default::default()
        };
        let built = build_presence_update(RtmDialect::Antelope, &bare).unwrap();
        assert_eq!(built.status, r#"{"presenceavailability":"away"}"#);
        assert_eq!(built.rich_presence.unwrap().game, "Idle");
    }

    #[test]
    fn subscribe_all_friends_round_trips_inside_a_frame() {
        let comm = super::super::proto::Communication {
            v1: Some(super::super::proto::CommunicationV1 {
                request_id: "c-0".into(),
                body: Some(communication_v1::Body::PresenceSubscribeAllFriendsV1(
                    PresenceSubscribeAllFriendsV1 {},
                )),
            }),
        };
        let bytes = comm.encode_to_vec();
        let decoded = super::super::proto::Communication::decode(bytes.as_slice()).unwrap();
        let body = decoded.v1.unwrap().body.unwrap();
        assert!(matches!(
            body,
            communication_v1::Body::PresenceSubscribeAllFriendsV1(_)
        ));
        // tag for field 51, wire type 2: (51 << 3) | 2 = 410 -> varint 0x9a 0x03
        assert!(bytes.windows(2).any(|w| w == [0x9a, 0x03]));
    }

    #[test]
    fn presence_from_rtm_keeps_game_and_status() {
        let presence = PresenceV1 {
            rich_presence: Some(RichPresenceV1 {
                game: "Titanfall 2: In the menus".into(),
                custom_rich_presence_data: r#"{"gameProductId":"Origin.OFR.50.0001456","version":1}"#.into(),
                ..Default::default()
            }),
            basic_presence_type: BasicPresenceType::Online as i32,
            ..Default::default()
        };
        let rich = RichPresence::from(&presence);
        assert_eq!(*rich.basic(), BasicPresence::Online);
        assert_eq!(rich.status(), "Titanfall 2: In the menus");
        assert_eq!(rich.game().as_deref(), Some("Origin.OFR.50.0001456"));
    }
}
