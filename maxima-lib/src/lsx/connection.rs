use derive_getters::Getters;
use lazy_static::lazy_static;
use log::{debug, error, warn};
use quick_xml::DeError;
use regex::Regex;
use std::{io::ErrorKind, path::PathBuf, sync::Arc};
use sysinfo::{Pid, PidExt, ProcessExt, System, SystemExt};
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        MutexGuard, RwLock,
    },
};

use super::{
    frame::FrameReader,
    request::{
        account::handle_query_entitlements_request,
        auth::handle_auth_code_request,
        challenge::handle_challenge_response,
        config::handle_config_request,
        core::{
            handle_connectivity_request, handle_set_downloader_util_request,
            handle_settings_request,
        },
        game::{handle_all_game_info_request, handle_game_info_request},
        igo::handle_show_igo_window_request,
        license::handle_license_request,
        offer::handle_query_offers_request,
        profile::{
            handle_get_block_list_request, handle_presence_request, handle_profile_request,
            handle_query_friends_request, handle_query_image_request,
            handle_query_presence_request, handle_set_presence_request,
        },
        progressive_install::{handle_pi_availability_request, handle_pi_installed_chunks_request},
        voip::handle_voip_status_request,
    },
    types::{
        create_lsx_message, LSXChallenge, LSXErrorSuccess, LSXEvent, LSXEventType, LSXMessageType,
        LSXRequest, LSXResponse, LSXResponseType, LSX,
    },
};
use crate::{
    core::{
        auth::storage::TokenError, launch::ActiveGameContext, LockedMaxima, Maxima, MaximaEvent,
    },
    lsx::{request::LSXRequestError, types::LSXRequestType},
    util::{
        native::NativeError,
        simple_crypto::{simple_decrypt, simple_encrypt},
    },
};

#[derive(Error, Debug)]
pub enum LSXConnectionError {
    #[error(transparent)]
    Xml(#[from] DeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Request(#[from] LSXRequestError),
    #[error(transparent)]
    Native(#[from] NativeError),

    #[error("LSX connection closed")]
    Closed,
    #[error("there is no active game context, LSX connection cannot be established")]
    GameContext,
    #[error("internal error in LSX connection: {0}")]
    Internal(ErrorKind),
}

const CORE_SENDER: &str = "EALS";

const CHALLENGE_BUILD: &str = "release";
const CHALLENGE_KEY: &str = "cacf897a20b6d612ad0c05e011df52bb"; // Need to figure out how to generate this
const CHALLENGE_VERSION: &str = "10,5,30,15625";

/// `ErrorSuccess` code for a request we can't answer (no handler, or the XML
/// doesn't map onto a request type we know). Code 0 is the success ack.
pub const LSX_ERROR_GENERIC: i64 = -1;

lazy_static! {
    static ref LSX_PATTERN: Regex = Regex::new(r"(?s)<LSX>.*?</LSX>").unwrap();
}

/// Dispatches an LSX request to its handler. Any request type without a
/// handler arm (a variant added to `LSXRequestType` before its handler
/// exists) gets the generic error response instead of failing the connection.
macro_rules! lsx_message_matcher {
    (
        $connection_var:expr, $message_var:expr, $message_type:ident;
        $($name:ident $handler:ident),* $(,)?
    ) => {
        match $message_var {
            $(
                $message_type::$name(msg) => $handler($connection_var, msg).await,
            )*
            #[allow(unreachable_patterns)]
            other => Ok(Some(unhandled_request_response(<&'static str>::from(&other)))),
        }?
    };
}

fn generic_error_response(description: String) -> LSXResponseType {
    LSXResponseType::ErrorSuccess(LSXErrorSuccess {
        attr_Code: LSX_ERROR_GENERIC,
        attr_Description: description,
    })
}

fn unhandled_request_response(request_name: &str) -> LSXResponseType {
    warn!(
        "LSX request `{}` has no handler; replying with a generic error",
        request_name
    );
    generic_error_response(format!("{} is not implemented", request_name))
}

/// What could be recovered from a request whose body didn't map onto any
/// request type we know, enough to answer it.
#[derive(Debug, PartialEq, Eq)]
struct UnmappedRequest {
    recipient: String,
    id: String,
    name: String,
}

/// Pulls the envelope attributes and the request element name out of
/// `<LSX><Request recipient=".." id=".."><Name .../></Request></LSX>` without
/// needing the body to deserialize.
fn describe_unmapped_request(xml: &str) -> Option<UnmappedRequest> {
    use quick_xml::{events::Event as XmlEvent, Reader};

    let mut reader = Reader::from_str(xml);
    let mut envelope: Option<(String, String)> = None;

    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(element)) | Ok(XmlEvent::Empty(element)) => {
                let name = String::from_utf8_lossy(element.local_name().as_ref()).into_owned();
                match &envelope {
                    None if name == "LSX" => {}
                    None if name == "Request" => {
                        let mut recipient = None;
                        let mut id = None;
                        for attr in element.attributes().flatten() {
                            let Ok(value) = attr.unescape_value() else {
                                continue;
                            };
                            match attr.key.local_name().as_ref() {
                                b"recipient" => recipient = Some(value.into_owned()),
                                b"id" => id = Some(value.into_owned()),
                                _ => {}
                            }
                        }
                        envelope = Some((recipient.unwrap_or_default(), id?));
                    }
                    None => return None,
                    Some((recipient, id)) => {
                        return Some(UnmappedRequest {
                            recipient: recipient.clone(),
                            id: id.clone(),
                            name,
                        })
                    }
                }
            }
            Ok(XmlEvent::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

fn preview(text: &str) -> String {
    const LIMIT: usize = 200;
    match text.char_indices().nth(LIMIT) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

pub enum EncryptionState {
    Disabled,
    Ready([u8; 16]),
    Enabled([u8; 16]),
}

#[derive(Getters)]
pub struct ConnectionState {
    #[getter(skip)]
    maxima: LockedMaxima,
    challenge: String,
    encryption: EncryptionState,
    pid: u32,
    /// Game version reported by the client in the LSX challenge response.
    /// Captured during challenge so subsequent handlers (e.g. GetAllGameInfo)
    /// can reflect the real version back; some games compare
    /// InstalledVersion / AvailableVersion against their own and treat a
    /// mismatch as a tampered install.
    game_version: Option<String>,
    /// Title reported by the client in the LSX challenge response. Used for diagnostic output and reflected in
    /// GetAllGameInfoResponse.
    game_title: Option<String>,
    /// Encoded (serialized, encrypted if enabled, NUL-terminated) messages
    /// for this connection's writer task.
    #[getter(skip)]
    outbound: UnboundedSender<String>,
}

pub type LockedConnectionState = Arc<RwLock<ConnectionState>>;

impl ConnectionState {
    /// Enable encryption on the packet after next
    pub fn enable_encryption(&mut self, encryption_key: [u8; 16]) {
        self.encryption = EncryptionState::Ready(encryption_key);
    }

    pub fn set_game_metadata(&mut self, version: String, title: String) {
        self.game_version = Some(version);
        self.game_title = Some(title);
    }

    pub async fn maxima(&mut self) -> MutexGuard<'_, Maxima> {
        self.maxima.lock().await
    }

    pub fn maxima_arc(&mut self) -> LockedMaxima {
        self.maxima.clone()
    }

    pub async fn access_token(&mut self) -> Result<String, TokenError> {
        self.maxima().await.access_token().await
    }

    pub fn queue_message(&mut self, message: LSX) -> Result<(), LSXConnectionError> {
        let mut str = quick_xml::se::to_string(&message)?;
        debug!("Queuing LSX Message: {}", str);

        if let EncryptionState::Enabled(key) = self.encryption {
            str = simple_encrypt(str.as_bytes(), &key)
        };

        str += "\0";
        self.outbound
            .send(str)
            .map_err(|_| LSXConnectionError::Closed)
    }

    /// Ready -> Enabled once the reply that is still sent in the clear (the
    /// `ChallengeAccepted`) has been queued.
    fn activate_pending_encryption(&mut self) {
        if let EncryptionState::Ready(key) = self.encryption {
            self.encryption = EncryptionState::Enabled(key);
        }
    }
}

pub fn get_os_pid(context: &ActiveGameContext) -> Result<u32, NativeError> {
    let mut pid = None;

    let sys = System::new_all();
    for e in sys.processes() {
        let (p_pid, process) = e;
        if process.cmd().is_empty() {
            continue;
        }

        let mut cmd = process.cmd()[0].to_owned();

        // Wine path handling
        if cfg!(unix) && cmd.starts_with("Z:") {
            cmd = cmd.replace("Z:", "").replace('\\', "/");
        }

        if !cmd.starts_with(context.game_path()) {
            continue;
        }

        for ele in process.environ() {
            let (key, value) = ele.split_once('=').unwrap_or((ele, ""));
            if key != "MXLaunchId" || value != context.launch_id() {
                continue;
            }

            pid = Some(p_pid.as_u32());
            break;
        }
    }

    Ok(pid.unwrap_or(0))
}

#[cfg(target_os = "windows")]
pub async fn get_wine_pid(
    _launch_id: &str,
    _name: &str,
    _wine_prefix: Option<&std::path::Path>,
) -> Result<u32, NativeError> {
    Ok(0)
}

#[cfg(target_os = "linux")]
pub async fn get_wine_pid(
    launch_id: &str,
    name: &str,
    wine_prefix: Option<&std::path::Path>,
) -> Result<u32, NativeError> {
    use crate::core::background_service::wine_get_pid;

    wine_get_pid(launch_id, name, wine_prefix).await
}

// macOS: no wine-helper.exe / background service; PID lookup is only used for
// Kyber DLL injection, which Wine on macOS can't do anyway. 0 = "not found",
// same contract as the windows stub above.
#[cfg(target_os = "macos")]
pub async fn get_wine_pid(
    _launch_id: &str,
    _name: &str,
    _wine_prefix: Option<&std::path::Path>,
) -> Result<u32, NativeError> {
    Ok(0)
}


/// One accepted LSX client. Owns the socket, and runs on its own task
/// (see [`Connection::run`]); nothing here is shared with other connections
/// except the `Maxima` behind [`LockedMaxima`].
pub struct Connection {
    stream: TcpStream,
    state: LockedConnectionState,
    outbound: UnboundedReceiver<String>,
}

impl Connection {
    pub async fn new(
        maxima_arc: LockedMaxima,
        stream: TcpStream,
    ) -> Result<Self, LSXConnectionError> {
        stream.set_nodelay(true)?;

        let mut pid: Result<u32, NativeError> = Ok(0);

        let maxima: MutexGuard<'_, Maxima> = maxima_arc.lock().await;
        match maxima.playing() {
            None => {
                // Game was launched externally (e.g. through Steam's
                // `applaunch` or a launcher) rather than through
                // `maxima-cli launch`. Accept the connection anyway —
                // LSX only needs the TCP socket; the PID/Kyber path is skipped
                // because there is no ActiveGameContext to interrogate.
                //
                // Without this, a game launched via Steam would have its LSX
                // connection rejected immediately, preventing online play
                // even when Maxima is running in the background.
                //
                // Ported from catornot/Maxima@patch-external-lsx, which itself
                // originated as upstream PR #42 (p0358).
                warn!("External LSX connection (the game was not started through Maxima)");
            }
            Some(context) => {
                // The PID system is mainly for Kyber injection
                pid = get_os_pid(context);
                if cfg!(unix) {
                    if let Ok(os_pid) = pid {
                        let sys = System::new_all();
                        if let Some(process) = sys.process(Pid::from_u32(os_pid)) {
                            let filename = PathBuf::from(
                                process.cmd()[0]
                                    .to_owned()
                                    .replace("Z:", "")
                                    .replace('\\', "/"),
                            )
                            .file_name()
                            .ok_or(NativeError::FileName)?
                            .to_str()
                            .ok_or(NativeError::Stringify)?
                            .to_owned();

                            pid = get_wine_pid(
                                &context.launch_id(),
                                &filename,
                                context.wine_prefix().as_deref(),
                            )
                            .await;
                        } else {
                            warn!(
                                "Failed to find game process while looking for PID {}",
                                os_pid
                            );
                        }
                    }
                }

                match &pid {
                    Err(err) => warn!("Error while finding game PID: {}", err),
                    Ok(0) => warn!("Failed to find PID through launch ID, things may not work!"),
                    _ => {}
                }
            }
        };
        drop(maxima);

        let (outbound_tx, outbound) = unbounded_channel();
        let state = Arc::new(RwLock::new(ConnectionState {
            maxima: maxima_arc,
            challenge: CHALLENGE_KEY.to_string(),
            encryption: EncryptionState::Disabled,
            pid: pid.unwrap_or(0),
            game_version: None,
            game_title: None,
            outbound: outbound_tx,
        }));

        Ok(Self {
            stream,
            state,
            outbound,
        })
    }

    // Initialization

    pub async fn send_challenge(&mut self) -> Result<(), LSXConnectionError> {
        let challenge = create_lsx_message(LSXMessageType::Event(LSXEvent {
            sender: CORE_SENDER.to_string(),
            value: LSXEventType::Challenge(LSXChallenge {
                attr_build: CHALLENGE_BUILD.to_string(),
                attr_key: self.state.read().await.challenge.to_owned(),
                attr_version: CHALLENGE_VERSION.to_string(),
            }),
        }));

        self.state.write().await.queue_message(challenge)?;
        Ok(())
    }

    /// Serves the connection until the peer goes away: a writer task drains
    /// the outbound queue to the socket while this task reads frames and
    /// dispatches them. Returns `Closed` for a clean EOF, anything else is
    /// the transport failure that ended the connection.
    pub async fn run(self) -> Result<(), LSXConnectionError> {
        let Connection {
            stream,
            state,
            mut outbound,
        } = self;
        let (read_half, mut write_half) = stream.into_split();

        let mut writer = tokio::spawn(async move {
            while let Some(message) = outbound.recv().await {
                write_half.write_all(message.as_bytes()).await?;
                write_half.flush().await?;
            }
            Ok::<(), std::io::Error>(())
        });

        let mut frames = FrameReader::new(read_half);
        let result = loop {
            tokio::select! {
                outcome = &mut writer => {
                    break match outcome {
                        Ok(Ok(())) => Err(LSXConnectionError::Closed),
                        Ok(Err(err)) => Err(err.into()),
                        Err(_) => Err(LSXConnectionError::Internal(ErrorKind::Other)),
                    };
                }
                frame = frames.next_frame() => match frame {
                    Ok(Some(frame)) => Connection::process_frame(&state, frame).await,
                    Ok(None) => break Err(LSXConnectionError::Closed),
                    Err(err) => break Err(err.into()),
                },
            }
        };

        writer.abort();
        result
    }

    // Message Processing

    async fn process_frame(state: &LockedConnectionState, frame: Vec<u8>) {
        let message = match state.read().await.encryption {
            EncryptionState::Enabled(key) => simple_decrypt(&frame, &key),
            _ => String::from_utf8_lossy(&frame).trim().to_owned(),
        };

        let mut found = false;
        for mat in LSX_PATTERN.find_iter(message.as_str()) {
            found = true;
            Connection::process_message(state, mat.as_str()).await;
        }

        if !found && !frame.iter().all(|b| b.is_ascii_whitespace()) {
            warn!(
                "Ignoring LSX frame without a message ({} bytes): {}",
                frame.len(),
                preview(&message)
            );
        }
    }

    async fn process_message(state: &LockedConnectionState, message: &str) {
        debug!("Received LSX Message: {}", message);

        let cleaned = message.replace("version=\"\" ", "");
        let lsx_message: LSX = match quick_xml::de::from_str(cleaned.as_str()) {
            Ok(lsx_message) => lsx_message,
            Err(err) => {
                Connection::reply_unmapped(state, message, err).await;
                return;
            }
        };

        match lsx_message.value {
            LSXMessageType::Event(_) => {}
            LSXMessageType::Response(_) => {
                warn!("Ignoring unexpected LSX response message from the game");
            }
            LSXMessageType::Request(request) => {
                // The challenge response switches the codec for everything
                // after it, so it has to be fully handled before the next
                // frame is decoded. Everything else runs concurrently so a
                // slow handler (EA network calls) doesn't hold up the rest.
                if matches!(request.value, LSXRequestType::ChallengeResponse(_)) {
                    Connection::handle_request(state, request).await;
                } else {
                    let state = state.clone();
                    tokio::spawn(async move {
                        Connection::handle_request(&state, request).await;
                    });
                }
            }
        }
    }

    /// A request we couldn't deserialize (a type we don't implement, e.g.
    /// `QueryAchievements`, or attributes that don't fit our types) still
    /// gets an answer if we can tell which request it was, so the game isn't
    /// left waiting on a reply that never comes.
    async fn reply_unmapped(state: &LockedConnectionState, message: &str, cause: DeError) {
        let Some(request) = describe_unmapped_request(message) else {
            warn!(
                "Ignoring unparseable LSX message ({}): {}",
                cause,
                preview(message)
            );
            return;
        };

        warn!(
            "LSX request `{}` (id {}) is not implemented or could not be mapped ({}); \
             replying with a generic error",
            request.name, request.id, cause
        );

        let reply = LSX {
            value: LSXMessageType::Response(LSXResponse {
                sender: request.recipient,
                id: request.id,
                value: generic_error_response(format!("{} is not implemented", request.name)),
            }),
        };
        Connection::queue_reply(state, reply).await;
    }

    async fn handle_request(state: &LockedConnectionState, request: LSXRequest) {
        match Connection::process_request_message(state, request).await {
            Ok(Some(reply)) => {
                Connection::queue_reply(state, LSX { value: reply }).await;
            }
            Ok(None) => {}
            Err(err) => error!("Failed to process LSX message: {}", err),
        }

        state.write().await.activate_pending_encryption();
    }

    async fn queue_reply(state: &LockedConnectionState, reply: LSX) {
        match state.write().await.queue_message(reply) {
            Ok(()) => {}
            Err(LSXConnectionError::Closed) => {
                debug!("Dropping LSX reply, the connection is already closed")
            }
            Err(err) => error!("Failed to queue LSX message: {}", err),
        }
    }

    async fn process_request_message(
        state: &LockedConnectionState,
        message: LSXRequest,
    ) -> Result<Option<LSXMessageType>, LSXConnectionError> {
        {
            let (maxima, pid) = {
                let state = state.read().await;
                (state.maxima.clone(), *state.pid())
            };
            maxima
                .lock()
                .await
                .call_event(MaximaEvent::ReceivedLSXRequest(pid, message.value.clone()));
        }

        let result = lsx_message_matcher!(
            state.clone(), message.value, LSXRequestType;

            ChallengeResponse handle_challenge_response,
            GetBlockList handle_get_block_list_request,
            GetConfig handle_config_request,
            GetProfile handle_profile_request,
            GetSetting handle_settings_request,
            RequestLicense handle_license_request,
            GetGameInfo handle_game_info_request,
            GetAllGameInfo handle_all_game_info_request,
            GetInternetConnectedState handle_connectivity_request,
            IsProgressiveInstallationAvailable handle_pi_availability_request,
            AreChunksInstalled handle_pi_installed_chunks_request,
            GetAuthCode handle_auth_code_request,
            GetPresence handle_presence_request,
            SetPresence handle_set_presence_request,
            QueryOffers handle_query_offers_request,
            QueryPresence handle_query_presence_request,
            QueryFriends handle_query_friends_request,
            QueryEntitlements handle_query_entitlements_request,
            QueryImage handle_query_image_request,
            GetVoipStatus handle_voip_status_request,
            ShowIGOWindow handle_show_igo_window_request,
            SetDownloaderUtilization handle_set_downloader_util_request,
        );

        Ok(result.map(|result| {
            LSXMessageType::Response(LSXResponse {
                sender: message.recipient,
                id: message.id,
                value: result,
            })
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_a_request_with_an_unknown_body() {
        let xml = r#"<LSX><Request recipient="EALS" id="42"><QueryAchievements version="" UserId="1" AchievementSet="x"/></Request></LSX>"#;
        assert_eq!(
            describe_unmapped_request(xml),
            Some(UnmappedRequest {
                recipient: "EALS".into(),
                id: "42".into(),
                name: "QueryAchievements".into(),
            })
        );
    }

    #[test]
    fn describes_a_request_with_a_nested_body() {
        let xml = r#"<LSX><Request recipient="EALS" id="7"><Foo><Bar/></Foo></Request></LSX>"#;
        let request = describe_unmapped_request(xml).unwrap();
        assert_eq!((request.id.as_str(), request.name.as_str()), ("7", "Foo"));
    }

    #[test]
    fn cannot_describe_anything_that_is_not_a_request() {
        assert_eq!(
            describe_unmapped_request(r#"<LSX><Event sender="EALS"><Challenge/></Event></LSX>"#),
            None
        );
        assert_eq!(
            describe_unmapped_request(r#"<LSX><Request recipient="EALS"><Foo/></Request></LSX>"#),
            None,
            "without an id there is nothing to correlate a reply with"
        );
        assert_eq!(describe_unmapped_request("<LSX><Request"), None);
        assert_eq!(describe_unmapped_request("not xml"), None);
    }

    #[test]
    fn generic_error_is_a_nonzero_error_success() {
        match unhandled_request_response("QueryAchievements") {
            LSXResponseType::ErrorSuccess(error) => {
                assert_eq!(error.attr_Code, LSX_ERROR_GENERIC);
                assert_ne!(error.attr_Code, 0);
                assert!(error.attr_Description.contains("QueryAchievements"));
            }
            other => panic!("unexpected response {:?}", other),
        }
    }
}
