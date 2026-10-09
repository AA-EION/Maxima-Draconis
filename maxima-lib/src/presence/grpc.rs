//! EA's "social" presence service over gRPC (upstream PR 67's `social` module).
//!
//! Only built with the `presence-grpc` feature. Flow: open a channel to
//! `api.k.social.ea.com`, create a presence session, subscribe to the friends'
//! presence, then hold a server-streaming `ConnectToPresenceSession` call open
//! for incoming notifications while local presence goes out through
//! `PartialUpdatePresenceSession`. If the stream dies the session is rebuilt
//! with exponential backoff, and the last local presence is re-applied.
//!
//! The protobuf messages are the workspace's prost 0.12 types from
//! [`super::social`]; tonic is used purely as the HTTP/2 + gRPC framing layer
//! through a small codec ([`ProstCodec`]) instead of its generated clients,
//! which would drag in a second, incompatible prost.
//!
//! None of this has been exercised against the live service without an EA
//! account; the endpoint, the property keys and the availability values are
//! what PR 67's author captured from EA app.

use std::{
    collections::HashMap,
    marker::PhantomData,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use log::{info, warn};
use prost::Message;
use tokio::{sync::Mutex, task::JoinHandle, time};
use tonic::{
    client::Grpc,
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    codegen::http::uri::PathAndQuery,
    metadata::MetadataValue,
    transport::{Channel, ClientTlsConfig, Endpoint},
    Request, Status,
};

use super::{
    backend::{PresenceBackend, PresenceBackendKind},
    model::{PresenceSink, PresenceUpdate, RichPresence},
    social::{
        presence_from_notification, proto::eadp::common::v1::*, proto::eadp::social::presence::v1::*,
        proto_update,
    },
};
use crate::{
    core::auth::storage::{AuthError, LockedAuthStorage},
    rtm::{connection::next_reconnect_delay, RtmError},
};

pub const DEFAULT_ENDPOINT: &str = "https://api.k.social.ea.com";
pub const ENDPOINT_ENV: &str = "MAXIMA_PRESENCE_GRPC_ENDPOINT";

const PATH_CREATE_SESSION: &str = "/eadp.social.presence.v1.PresenceService/CreatePresenceSession";
const PATH_CONNECT_SESSION: &str = "/eadp.social.presence.v1.PresenceService/ConnectToPresenceSession";
const PATH_SUBSCRIBE_FRIENDS: &str =
    "/eadp.social.presence.v1.PresenceService/SubscribeToFriendsPresence";
const PATH_UPDATE_SESSION: &str =
    "/eadp.social.presence.v1.PresenceService/PartialUpdatePresenceSession";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const UNARY_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// A stream that stayed up this long counts as healthy and resets the backoff.
const HEALTHY_STREAM: Duration = Duration::from_secs(30);
/// Friend presences are event-driven, so re-touch the store before its TTL
/// would age out a friend whose presence simply hasn't changed.
const STORE_REFRESH: Duration = Duration::from_secs(120);

impl From<Status> for RtmError {
    fn from(status: Status) -> Self {
        RtmError::Presence(format!("gRPC {:?}: {}", status.code(), status.message()))
    }
}

impl From<tonic::transport::Error> for RtmError {
    fn from(err: tonic::transport::Error) -> Self {
        RtmError::Presence(format!("gRPC transport: {}", err))
    }
}

/// tonic codec for prost 0.12 messages.
pub(crate) struct ProstCodec<T, U>(PhantomData<(T, U)>);

impl<T, U> ProstCodec<T, U> {
    pub(crate) fn new() -> Self {
        Self(PhantomData)
    }
}

pub(crate) struct ProstEncoder<T>(PhantomData<T>);
pub(crate) struct ProstDecoder<U>(PhantomData<U>);

impl<T, U> Codec for ProstCodec<T, U>
where
    T: Message + Send + 'static,
    U: Message + Default + Send + 'static,
{
    type Encode = T;
    type Decode = U;
    type Encoder = ProstEncoder<T>;
    type Decoder = ProstDecoder<U>;

    fn encoder(&mut self) -> Self::Encoder {
        ProstEncoder(PhantomData)
    }

    fn decoder(&mut self) -> Self::Decoder {
        ProstDecoder(PhantomData)
    }
}

impl<T: Message + Send + 'static> Encoder for ProstEncoder<T> {
    type Item = T;
    type Error = Status;

    fn encode(&mut self, item: T, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        item.encode(dst)
            .map_err(|err| Status::internal(format!("failed to encode request: {}", err)))
    }
}

impl<U: Message + Default + Send + 'static> Decoder for ProstDecoder<U> {
    type Item = U;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<U>, Status> {
        U::decode(src)
            .map(Some)
            .map_err(|err| Status::internal(format!("failed to decode response: {}", err)))
    }
}

fn authed<T>(token: &str, message: T) -> Result<Request<T>, RtmError> {
    let mut request = Request::new(message);
    let value = MetadataValue::try_from(format!("Bearer {}", token))
        .map_err(|_| RtmError::Presence("access token is not a valid header value".to_owned()))?;
    request.metadata_mut().insert("authorization", value);
    Ok(request)
}

async fn access_token(auth: &LockedAuthStorage) -> Result<String, RtmError> {
    Ok(auth
        .lock()
        .await
        .access_token()
        .await?
        .ok_or(AuthError::NoAuthCode)?)
}

fn grpc_for(channel: &Channel) -> Grpc<Channel> {
    Grpc::new(channel.clone()).max_decoding_message_size(MAX_MESSAGE_SIZE)
}

async fn ready(grpc: &mut Grpc<Channel>) -> Result<(), Status> {
    grpc.ready()
        .await
        .map_err(|err| Status::unknown(format!("service was not ready: {}", err)))
}

async fn unary<Req, Resp>(
    channel: &Channel,
    bearer: &str,
    path: &'static str,
    what: &str,
    message: Req,
) -> Result<Resp, RtmError>
where
    Req: Message + Send + 'static,
    Resp: Message + Default + Send + 'static,
{
    let request = authed(bearer, message)?;
    let mut grpc = grpc_for(channel);
    let call = async {
        ready(&mut grpc).await?;
        grpc.unary(request, PathAndQuery::from_static(path), ProstCodec::<Req, Resp>::new())
            .await
    };
    match time::timeout(UNARY_TIMEOUT, call).await {
        Ok(response) => Ok(response?.into_inner()),
        Err(_) => Err(RtmError::Presence(format!("{} timed out", what))),
    }
}

/// Create a presence session and subscribe it to the friends' presence.
async fn establish(
    auth: &LockedAuthStorage,
    channel: &Channel,
) -> Result<PresenceSessionToken, RtmError> {
    let bearer = access_token(auth).await?;

    let created: CreatePresenceSessionResponse = unary(
        channel,
        &bearer,
        PATH_CREATE_SESSION,
        "create presence session",
        CreatePresenceSessionRequest {
            client_info: Some(ClientInfo {
                player_network_id: Some(PlayerNetworkId::ea()),
                product_id: Some(ProductId::juno()),
                device_platform_id: Some(DevicePlatformId::pc()),
                locale: "en_US".to_owned(),
            }),
        },
    )
    .await?;

    let token = created
        .presence_session_token
        .ok_or_else(|| RtmError::Presence("presence session had no token".to_owned()))?;

    let _: SubscribeToFriendsPresenceResponse = unary(
        channel,
        &bearer,
        PATH_SUBSCRIBE_FRIENDS,
        "subscribe to friends presence",
        SubscribeToFriendsPresenceRequest {
            presence_session_token: Some(token.clone()),
        },
    )
    .await?;

    Ok(token)
}

async fn push_update(
    auth: &LockedAuthStorage,
    channel: &Channel,
    token: &PresenceSessionToken,
    update: &PresenceUpdate,
) -> Result<(), RtmError> {
    let bearer = access_token(auth).await?;
    let _: PartialUpdatePresenceSessionResponse = unary(
        channel,
        &bearer,
        PATH_UPDATE_SESSION,
        "update presence",
        PartialUpdatePresenceSessionRequest {
            presence_session_token: Some(token.clone()),
            presence_update: Some(proto_update(update)),
        },
    )
    .await?;
    Ok(())
}

/// Hold one session's notification stream open until it ends or fails.
async fn run_stream(
    auth: &LockedAuthStorage,
    channel: &Channel,
    token: &PresenceSessionToken,
    sink: &PresenceSink,
    known: &mut HashMap<String, RichPresence>,
) -> Result<(), RtmError> {
    let bearer = access_token(auth).await?;
    let request = authed(
        &bearer,
        ConnectToPresenceSessionRequest {
            presence_session_token: Some(token.clone()),
            presence: Vec::new(),
        },
    )?;

    let mut grpc = grpc_for(channel);
    let connect = async {
        ready(&mut grpc).await?;
        grpc.server_streaming(
            request,
            PathAndQuery::from_static(PATH_CONNECT_SESSION),
            ProstCodec::<ConnectToPresenceSessionRequest, ConnectToPresenceSessionResponse>::new(),
        )
        .await
    };
    let mut stream = match time::timeout(UNARY_TIMEOUT, connect).await {
        Ok(response) => response?.into_inner(),
        Err(_) => {
            return Err(RtmError::Presence(
                "connecting to the presence session timed out".to_owned(),
            ))
        }
    };

    let mut refresh = time::interval(STORE_REFRESH);
    refresh.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    refresh.tick().await;

    loop {
        tokio::select! {
            message = stream.message() => {
                let Some(response) = message? else { return Ok(()) };
                let Some(notification) = response.presence_notification else { continue };
                if let Some((id, presence)) = presence_from_notification(&notification) {
                    known.insert(id.clone(), presence.clone());
                    sink.publish(id, presence).await;
                }
            }
            _ = refresh.tick() => {
                for (id, presence) in known.iter() {
                    sink.refresh(id.clone(), presence.clone()).await;
                }
            }
        }
    }
}

#[derive(Default)]
struct Shared {
    session: Mutex<Option<PresenceSessionToken>>,
    /// What the local user last asked for, replayed after a reconnect.
    last_update: Mutex<Option<PresenceUpdate>>,
}

/// Keeps the notification stream alive for the life of the backend, rebuilding
/// the session with backoff whenever it drops.
async fn supervise(
    auth: LockedAuthStorage,
    channel: Channel,
    shared: Arc<Shared>,
    sink: PresenceSink,
    mut token: PresenceSessionToken,
) {
    let mut known: HashMap<String, RichPresence> = HashMap::new();
    let mut delay = INITIAL_RECONNECT_DELAY;

    loop {
        let started = Instant::now();
        match run_stream(&auth, &channel, &token, &sink, &mut known).await {
            Ok(()) => info!("Presence stream closed by the server"),
            Err(err) => {
                warn!("Presence stream failed: {}", err);
                sink.report_error(err.to_string());
            }
        }
        if started.elapsed() >= HEALTHY_STREAM {
            delay = INITIAL_RECONNECT_DELAY;
        }

        loop {
            time::sleep(delay).await;
            delay = next_reconnect_delay(delay);

            match establish(&auth, &channel).await {
                Ok(fresh) => {
                    info!("Presence session re-established");
                    *shared.session.lock().await = Some(fresh.clone());
                    token = fresh;

                    let last = shared.last_update.lock().await.clone();
                    if let Some(update) = last {
                        if let Err(err) = push_update(&auth, &channel, &token, &update).await {
                            warn!("Failed to restore presence after reconnect: {}", err);
                        }
                    }
                    break;
                }
                Err(err) => {
                    warn!("Presence reconnect failed: {}", err);
                    sink.report_error(err.to_string());
                }
            }
        }
    }
}

pub struct GrpcPresenceBackend {
    auth: LockedAuthStorage,
    sink: PresenceSink,
    shared: Arc<Shared>,
    channel: Option<Channel>,
    task: Option<JoinHandle<()>>,
}

impl GrpcPresenceBackend {
    pub fn new(auth: LockedAuthStorage, sink: PresenceSink) -> Self {
        Self {
            auth,
            sink,
            shared: Arc::new(Shared::default()),
            channel: None,
            task: None,
        }
    }

    async fn connect_channel() -> Result<Channel, RtmError> {
        let url = std::env::var(ENDPOINT_ENV).unwrap_or_else(|_| DEFAULT_ENDPOINT.to_owned());
        let endpoint = Endpoint::from_shared(url)
            .map_err(|err| RtmError::Presence(format!("invalid presence endpoint: {}", err)))?
            .tls_config(ClientTlsConfig::new().with_webpki_roots())?
            .connect_timeout(CONNECT_TIMEOUT)
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(20))
            .keep_alive_while_idle(true);

        match time::timeout(CONNECT_TIMEOUT, endpoint.connect()).await {
            Ok(channel) => Ok(channel?),
            Err(_) => Err(RtmError::Presence(
                "connecting to presence timed out".to_owned(),
            )),
        }
    }

    fn stop_task(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for GrpcPresenceBackend {
    fn drop(&mut self) {
        self.stop_task();
    }
}

#[async_trait]
impl PresenceBackend for GrpcPresenceBackend {
    fn kind(&self) -> PresenceBackendKind {
        PresenceBackendKind::Grpc
    }

    fn sink(&self) -> &PresenceSink {
        &self.sink
    }

    async fn login(&mut self) -> Result<(), RtmError> {
        self.stop_task();

        let channel = match self.channel.clone() {
            Some(channel) => channel,
            None => {
                let channel = Self::connect_channel().await?;
                self.channel = Some(channel.clone());
                channel
            }
        };

        let token = establish(&self.auth, &channel).await?;
        *self.shared.session.lock().await = Some(token.clone());
        info!("Logged into gRPC presence");

        self.task = Some(tokio::spawn(supervise(
            self.auth.clone(),
            channel,
            self.shared.clone(),
            self.sink.clone(),
            token,
        )));
        Ok(())
    }

    /// Friends are subscribed server-side when the session is created.
    async fn subscribe(&mut self, _friend_ids: &[String]) -> Result<(), RtmError> {
        Ok(())
    }

    async fn set_presence(&mut self, update: &PresenceUpdate) -> Result<(), RtmError> {
        *self.shared.last_update.lock().await = Some(update.clone());

        let token = self
            .shared
            .session
            .lock()
            .await
            .clone()
            .ok_or(RtmError::NotLoggedIn)?;
        let channel = self.channel.as_ref().ok_or(RtmError::NotLoggedIn)?;

        info!("Updating gRPC presence to '{}'", update.legacy_status());
        push_update(&self.auth, channel, &token, update).await
    }

    /// The HTTP/2 keep-alive does this for us.
    async fn heartbeat(&mut self) -> Result<(), RtmError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_errors_become_presence_errors() {
        let err: RtmError = Status::unauthenticated("bad token").into();
        let text = err.to_string();
        assert!(text.contains("Unauthenticated"), "{}", text);
        assert!(text.contains("bad token"), "{}", text);
    }

    #[test]
    fn bearer_header_rejects_invalid_tokens() {
        assert!(authed("fine-token", ()).is_ok());
        assert!(authed("bad\ntoken", ()).is_err());
    }

    #[test]
    fn method_paths_are_valid_uris() {
        for path in [
            PATH_CREATE_SESSION,
            PATH_CONNECT_SESSION,
            PATH_SUBSCRIBE_FRIENDS,
            PATH_UPDATE_SESSION,
        ] {
            assert!(path.parse::<PathAndQuery>().is_ok(), "{}", path);
        }
    }
}
