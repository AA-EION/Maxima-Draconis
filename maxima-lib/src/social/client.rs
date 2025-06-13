use super::eadp::{
    common::v1::{DevicePlatformId, PlayerNetworkId, ProductId},
    social::presence::v1::{
        presence_service_client::PresenceServiceClient,
        presence_session_notification::PropertiesEnt, value, ClientInfo,
        ConnectToPresenceSessionRequest, ConnectToPresenceSessionResponse,
        CreatePresenceSessionRequest, CreatePresenceSessionResponse, PresenceSessionToken,
        PresenceUpdate, SubscribeToFriendsPresenceRequest, Value,
    },
};
use super::SocialError;
use crate::core::auth::storage::LockedAuthStorage;
use derive_builder::Builder;
use derive_getters::Getters;
use futures::stream::Next;
use futures::task::noop_waker_ref;
use futures::{FutureExt, StreamExt, TryFuture};
use std::collections::HashMap;
use std::future::Future;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::lsx::types::LSXPresence;
use crate::social::eadp::social::presence::v1::value::Value::{IntegerValue, StringValue};
use log::{debug, error, info};
use rustls::ClientConfig;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;
use tonic::transport::{Channel, ClientTlsConfig};
use tonic::{Request, Response, Status, Streaming};
use webpki_roots::TLS_SERVER_ROOTS;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UserPresenceBasic {
    Unknown,
    Offline,
    Online,
    Away,
}

impl Into<LSXPresence> for UserPresenceBasic {
    fn into(self) -> LSXPresence {
        match self {
            UserPresenceBasic::Offline => LSXPresence::Offline,
            UserPresenceBasic::Online => LSXPresence::Online,
            UserPresenceBasic::Away => LSXPresence::Idle,
            _ => LSXPresence::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub struct UserPresence {
    pub offer_id: Option<String>,
    pub multiplayer_id: Option<String>,
    pub rich_presence: Option<String>,
    pub game_presence: Option<String>,
    pub basic: UserPresenceBasic,

    pub group_name: Option<String>,
    pub group_id: Option<String>,
    pub group_public: Option<bool>,
    pub joinable: Option<bool>,
    pub joinable_invite_only: Option<bool>,
}

impl Default for UserPresence {
    fn default() -> Self {
        Self {
            offer_id: None,
            multiplayer_id: None,
            rich_presence: None,
            game_presence: None,
            basic: UserPresenceBasic::Unknown,

            group_name: None,
            group_id: None,
            group_public: None,
            joinable: None,
            joinable_invite_only: None,
        }
    }
}

#[derive(Clone)]
pub enum SocialEvent {
    FriendPresence { id: String, presence: UserPresence },
    Error(SocialError),
}

pub enum SocialRequest {
    Shutdown,
}

#[derive(Getters)]
pub struct SocialClient {
    pub tx: Sender<SocialRequest>,
    backlog: Arc<Mutex<Vec<SocialEvent>>>,
    senders: Arc<Mutex<Vec<Sender<SocialEvent>>>>,
    pub presence_store: Arc<Mutex<HashMap<String, UserPresence>>>,
}

impl SocialClient {
    pub fn new(auth: LockedAuthStorage) -> Self {
        let (tx, social_rx) = std::sync::mpsc::channel();
        let (social_tx, rx) = std::sync::mpsc::channel();

        let senders = Arc::new(Mutex::new(Vec::new()));
        let senders_burn = senders.clone();

        let backlog = Arc::new(Mutex::new(Vec::new()));
        let backlog_burn = backlog.clone();

        let presence_store = Arc::new(Mutex::new(HashMap::new()));
        let presence_store_burn = presence_store.clone();

        tokio::task::spawn(async move {
            let fallback_tx = social_tx.clone();
            match SocialClient::run(
                auth,
                senders_burn,
                backlog_burn,
                presence_store_burn,
                social_rx,
            )
            .await
            {
                Ok(_) => (),
                Err(e) => {
                    error!("Social client error: {}", e);
                    let _ = fallback_tx.send(SocialEvent::Error(e));
                }
            }
        });

        SocialClient {
            tx,
            backlog,
            senders,
            presence_store,
        }
    }

    // TODO(headassbtw): replace this with a normal SocialRequest once the server is polled and not awaited?
    pub async fn subscribe(&mut self) -> Receiver<SocialEvent> {
        let (a, b) = std::sync::mpsc::channel::<SocialEvent>();
        for event in self.backlog.lock().await.iter() {
            let _ = a.send(event.clone());
        }
        self.senders.lock().await.push(a);
        b
    }

    async fn run(
        auth: LockedAuthStorage,
        senders: Arc<Mutex<Vec<Sender<SocialEvent>>>>,
        backlog: Arc<Mutex<Vec<SocialEvent>>>,
        presence_store: Arc<Mutex<HashMap<String, UserPresence>>>,
        rx: Receiver<SocialRequest>,
    ) -> Result<(), SocialError> {
        let token = {
            let auth = auth.lock().await.access_token().await.unwrap().unwrap();
            format!("Bearer {}", auth)
        };
        let config = ClientTlsConfig::default().trust_anchors(TLS_SERVER_ROOTS.to_owned());
        let channel = Channel::from_static("https://api.k.social.ea.com")
            .tls_config(config)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client =
            PresenceServiceClient::with_interceptor(channel, move |mut req: Request<()>| {
                req.metadata_mut()
                    .insert("authorization", token.parse().unwrap());
                Ok(req)
            });

        let request = tonic::Request::new(CreatePresenceSessionRequest {
            client_info: Some(ClientInfo {
                player_network_id: Some(PlayerNetworkId::ea()),
                product_id: Some(ProductId::juno()),
                device_platform_id: Some(DevicePlatformId::pc()),
                locale: "en_US".to_owned(),
            }),
        });

        let res = client.create_presence_session(request).await.unwrap();
        let token = res.into_inner().presence_session_token;
        let res = client
            .subscribe_to_friends_presence(SubscribeToFriendsPresenceRequest {
                presence_session_token: token.clone(),
            })
            .await
            .unwrap();

        let req = ConnectToPresenceSessionRequest {
            presence_session_token: token.clone(),
            presence: Vec::new(),
        };

        'guh: loop {
            // TODO(headassbtw): poll the future
            let mut resp = client
                .connect_to_presence_session(req.clone())
                .await
                .unwrap()
                .into_inner();
            while let Some(guh) = resp.next().await {
                if let Ok(presence) = guh {
                    let presence = presence.presence_notification.unwrap();
                    let id = presence.player_id.unwrap().id;
                    let mut user_presence = UserPresence::default();
                    if presence.player_online {
                        let presence = presence.session_notification.unwrap();
                        info!("{:?}", presence);

                        for prop in presence.properties {
                            let value = prop.value.unwrap().value.unwrap();
                            match prop.key.as_str() {
                                "ea_app.presenceAvailability" => {
                                    if let IntegerValue(val) = value {
                                        user_presence.basic = match val {
                                            -1 => UserPresenceBasic::Online,
                                            -2 => UserPresenceBasic::Away,
                                            _ => UserPresenceBasic::Unknown,
                                        };
                                    }
                                }
                                "ea_app.productId" => {
                                    if let StringValue(val) = value {
                                        user_presence.offer_id = Some(val);
                                    }
                                } // Offer ID
                                "ea_app.gameTitle" => {
                                    if let StringValue(val) = value {
                                        user_presence.game_presence = Some(val);
                                    }
                                } // "Battlefield 4"
                                "ea_app.richPresence" => {
                                    if let StringValue(val) = value {
                                        user_presence.rich_presence = Some(val);
                                    }
                                } // "In the menus"
                                "ea_app.presenceStatus" => {
                                    if let StringValue(val) = value {
                                        user_presence.game_presence = Some(val);
                                    }
                                } // "Battlefield 4 In the menus"
                                "ea_app.isJoinable" => {
                                    if let IntegerValue(val) = value {
                                        user_presence.joinable = Some(val > 0);
                                    }
                                } // unknown, safe to guess
                                "ea_app.isJoinableInviteOnly" => {
                                    if let IntegerValue(val) = value {
                                        user_presence.joinable_invite_only = Some(val > 0);
                                    }
                                } // unknown, safe to guess
                                "ea_app.multiplayerId" => {
                                    if let StringValue(val) = value {
                                        user_presence.multiplayer_id = Some(val);
                                    }
                                } // "1002645",
                                "ea_app.groupGuid" => {
                                    if let StringValue(value) = value {
                                        user_presence.group_id = Some(value);
                                    }
                                } // unknown
                                "ea_app.groupName" => {
                                    if let StringValue(val) = value {
                                        user_presence.group_name = Some(val);
                                    }
                                } // unknown
                                "ea_app.groupIsPublic" => {
                                    if let IntegerValue(val) = value {
                                        user_presence.group_public = Some(val > 0);
                                    }
                                } // unknown, safe to guess
                                "ea_app.presenceIsInvisible" => {} // this is always present, never used, and never honored.
                                "ea_app.gamePresence" => {}        // appears to be a Base64 string
                                "ea_app.gameSessionString" => {}   // unknown, probably a string
                                unhandled => {
                                    error!(
                                        "unhandled presence property `{} : {:?}`",
                                        unhandled, value
                                    );
                                }
                            }
                        }
                    }

                    presence_store
                        .lock()
                        .await
                        .insert(id.clone(), user_presence.clone());

                    let ev = SocialEvent::FriendPresence {
                        id,
                        presence: user_presence,
                    };
                    backlog.lock().await.push(ev.clone());
                    for tx in senders.lock().await.iter() {
                        let _ = tx.send(ev.clone());
                    }
                }
            }

            let a = rx.try_recv();
            if a.is_err() {
                continue;
            }
            match a.unwrap() {
                SocialRequest::Shutdown => {
                    drop(resp);
                    break 'guh;
                }
            }
            info!("Reconnecting to social client");
        }

        info!("Social client disconnected");
        Ok(())
    }
}
