use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
    time::Duration,
};

use log::{info, warn};
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};

use crate::lsx::connection::LSXConnectionError;
use crate::{core::LockedMaxima, lsx::connection::Connection};

#[derive(Error, Debug)]
pub enum LSXServerError {
    #[error(transparent)]
    Conn(#[from] LSXConnectionError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A bound, not yet serving, LSX listener.
pub struct LsxListener {
    listener: TcpListener,
    port: u16,
}

/// Binds the LSX listener on loopback. Passing port 0 asks the OS for a free
/// port; [`LsxListener::port`] reports what was actually bound.
pub async fn bind(port: u16) -> Result<LsxListener, LSXServerError> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    info!("Listening on: 127.0.0.1:{}", port);
    Ok(LsxListener { listener, port })
}

pub async fn start_server(port: u16, maxima: LockedMaxima) -> Result<(), LSXServerError> {
    bind(port).await?.serve(maxima).await
}

impl LsxListener {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Accepts connections forever, running each on its own task with its
    /// own state: a connection that errors, hangs or panics only takes down
    /// itself.
    pub async fn serve(self, maxima: LockedMaxima) -> Result<(), LSXServerError> {
        let connections = maxima.lock().await.lsx_connection_counter();

        loop {
            let (socket, addr) = match self.listener.accept().await {
                Ok(accepted) => accepted,
                Err(err) => {
                    // A connection that died in the accept queue, or running
                    // out of descriptors, is no reason to stop serving.
                    warn!("Failed to accept an LSX connection: {}", err);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };

            info!("New LSX connection: {:?}", addr);
            tokio::spawn(serve_connection(
                maxima.clone(),
                connections.clone(),
                socket,
                addr,
            ));
        }
    }
}

/// Counts a connection as live for as long as it exists. Lives in a plain
/// atomic (not behind the `Maxima` mutex) so it is released even if the
/// connection task panics.
struct LiveConnection(Arc<AtomicU16>);

impl LiveConnection {
    fn new(counter: Arc<AtomicU16>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter)
    }
}

impl Drop for LiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn serve_connection(
    maxima: LockedMaxima,
    counter: Arc<AtomicU16>,
    socket: TcpStream,
    addr: SocketAddr,
) {
    let mut connection = match Connection::new(maxima.clone(), socket).await {
        Ok(connection) => connection,
        Err(err) => {
            warn!("Failed to establish LSX connection: {}", err);
            return;
        }
    };

    if let Err(err) = connection.send_challenge().await {
        warn!("Failed to send LSX challenge to {}: {}", addr, err);
        return;
    }

    let _live = LiveConnection::new(counter);
    maxima.lock().await.set_player_started();

    match connection.run().await {
        // `Closed` is a clean EOF from the peer (the game closed the socket
        // on purpose); anything else is an unexpected transport failure that
        // may be the cause of an in-game error.
        Ok(()) | Err(LSXConnectionError::Closed) => info!("LSX connection closed: {}", addr),
        Err(err) => warn!("LSX connection closed: {} ({})", addr, err),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        time::timeout,
    };

    use super::*;
    use crate::{
        core::{Maxima, MaximaOptionsBuilder},
        lsx::types::{
            LSXChallengeResponse, LSXEventType, LSXGameInfoId, LSXGetAllGameInfo, LSXGetGameInfo,
            LSXMessageType,
            LSXRequest, LSXRequestType, LSXResponseType, LSX,
        },
        util::simple_crypto::{make_challenge_response, make_lsx_key, simple_decrypt, simple_encrypt},
    };

    const WAIT: Duration = Duration::from_secs(10);

    async fn test_maxima() -> LockedMaxima {
        Maxima::new_with_options(
            MaximaOptionsBuilder::default()
                .load_auth_storage(false)
                .dummy_local_user(true)
                .build()
                .unwrap(),
        )
        .await
        .unwrap()
    }

    async fn start() -> (LockedMaxima, u16) {
        let maxima = test_maxima().await;
        let listener = bind(0).await.unwrap();
        let port = listener.port();
        assert_ne!(port, 0);
        tokio::spawn(listener.serve(maxima.clone()));
        (maxima, port)
    }

    /// A minimal LSX client: reads NUL-terminated frames, encrypting and
    /// decrypting once the handshake switched the codec on.
    struct Client {
        stream: TcpStream,
        buf: Vec<u8>,
        key: Option<[u8; 16]>,
    }

    impl Client {
        async fn connect(port: u16) -> Client {
            Client {
                stream: TcpStream::connect(("127.0.0.1", port)).await.unwrap(),
                buf: Vec::new(),
                key: None,
            }
        }

        async fn raw_frame(&mut self) -> Option<String> {
            loop {
                if let Some(end) = self.buf.iter().position(|b| *b == 0) {
                    let frame: Vec<u8> = self.buf.drain(..=end).collect();
                    let frame = &frame[..frame.len() - 1];
                    return Some(match self.key {
                        Some(key) => simple_decrypt(frame, &key),
                        None => String::from_utf8_lossy(frame).into_owned(),
                    });
                }

                let mut chunk = [0u8; 4096];
                let n = timeout(WAIT, self.stream.read(&mut chunk))
                    .await
                    .expect("timed out waiting for the server")
                    .unwrap_or(0);
                if n == 0 {
                    return None;
                }
                self.buf.extend_from_slice(&chunk[..n]);
            }
        }

        async fn frame(&mut self) -> String {
            self.raw_frame().await.expect("server closed the connection")
        }

        async fn send_raw(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).await.unwrap();
        }

        async fn send(&mut self, request: LSXRequestType, id: &str) {
            let xml = quick_xml::se::to_string(&LSX {
                value: LSXMessageType::Request(LSXRequest {
                    recipient: "EALS".into(),
                    id: id.into(),
                    value: request,
                }),
            })
            .unwrap();
            self.send_xml(&xml).await;
        }

        async fn send_xml(&mut self, xml: &str) {
            let mut bytes = match self.key {
                Some(key) => simple_encrypt(xml.as_bytes(), &key).into_bytes(),
                None => xml.as_bytes().to_vec(),
            };
            bytes.push(0);
            self.send_raw(&bytes).await;
        }

        /// Plaintext challenge, plaintext reply, then everything encrypted.
        async fn handshake(&mut self, title: &str) {
            let challenge: LSX = quick_xml::de::from_str(&self.frame().await).unwrap();
            let LSXMessageType::Event(event) = challenge.value else {
                panic!("the server speaks first, with a challenge event");
            };
            let LSXEventType::Challenge(challenge) = event.value;

            let response = LSXChallengeResponse {
                attr_response: make_challenge_response(&challenge.attr_key),
                attr_key: "client-key".into(),
                attr_version: "2".into(),
                content_id: "1234".into(),
                title: title.into(),
                multiplayer_id: "5678".into(),
                language: "en_US".into(),
                version: "1.2.3.4".into(),
            };
            self.send(LSXRequestType::ChallengeResponse(response), "1").await;

            let accepted: LSX = quick_xml::de::from_str(&self.frame().await).unwrap();
            let LSXMessageType::Response(accepted) = accepted.value else {
                panic!("expected a response to the challenge response");
            };
            assert_eq!(accepted.id, "1");
            match accepted.value {
                LSXResponseType::ChallengeAccepted(accepted) => assert_eq!(
                    accepted.attr_response,
                    make_challenge_response("client-key")
                ),
                other => panic!("unexpected response {:?}", other),
            }

            // version "2" negotiates the fixed key.
            self.key = Some(make_lsx_key(0));
        }

        async fn game_info(&mut self, id: &str) -> String {
            self.send(
                LSXRequestType::GetGameInfo(LSXGetGameInfo {
                    attr_GameInfoId: LSXGameInfoId::InstalledLanguage,
                    attr_version: String::new(),
                }),
                id,
            )
            .await;

            let reply: LSX = quick_xml::de::from_str(&self.frame().await).unwrap();
            let LSXMessageType::Response(reply) = reply.value else {
                panic!("expected a response");
            };
            assert_eq!(reply.id, id);
            match reply.value {
                LSXResponseType::GetGameInfoResponse(info) => info.attr_GameInfo,
                other => panic!("unexpected response {:?}", other),
            }
        }
    }

    async fn wait_for_connections(maxima: &LockedMaxima, expected: u16) {
        timeout(WAIT, async {
            while maxima.lock().await.lsx_connection_count() != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("connection count never reached {}", expected);
        });
    }

    #[tokio::test]
    async fn challenge_handshake_then_encrypted_traffic() {
        let (maxima, port) = start().await;
        let mut client = Client::connect(port).await;

        client.handshake("TestGame").await;
        assert_eq!(client.game_info("2").await, "en_US");
        assert_eq!(client.game_info("3").await, "en_US");

        assert_eq!(maxima.lock().await.lsx_connection_count(), 1);
        assert_eq!(maxima.lock().await.lsx_bound_port(), None, "only start_lsx records it");
    }

    #[tokio::test]
    async fn challenge_response_is_only_accepted_with_the_right_secret() {
        let (_maxima, port) = start().await;
        let mut client = Client::connect(port).await;
        let _ = client.frame().await;

        let response = LSXChallengeResponse {
            attr_response: make_challenge_response("not the challenge"),
            attr_key: "client-key".into(),
            attr_version: "2".into(),
            content_id: String::new(),
            title: String::new(),
            multiplayer_id: String::new(),
            language: String::new(),
            version: String::new(),
        };
        client.send(LSXRequestType::ChallengeResponse(response), "1").await;

        // No ChallengeAccepted, and the connection stays plaintext and alive.
        let silence = timeout(Duration::from_millis(300), client.raw_frame()).await;
        assert!(silence.is_err(), "a bad challenge response must not be answered");
    }

    #[tokio::test]
    async fn two_clients_have_independent_state() {
        let (maxima, port) = start().await;
        let mut a = Client::connect(port).await;
        let mut b = Client::connect(port).await;

        // A negotiates encryption first; B is still in the clear.
        a.handshake("GameA").await;
        assert_eq!(a.game_info("2").await, "en_US");

        b.handshake("GameB").await;
        assert_eq!(b.game_info("2").await, "en_US");
        assert_eq!(a.game_info("3").await, "en_US");

        wait_for_connections(&maxima, 2).await;
    }

    #[tokio::test]
    async fn garbage_and_disconnects_do_not_break_other_clients() {
        let (maxima, port) = start().await;
        let mut healthy = Client::connect(port).await;
        healthy.handshake("Healthy").await;

        // Plaintext garbage before any handshake: not UTF-8, no message.
        let mut noisy = Client::connect(port).await;
        let _ = noisy.frame().await;
        noisy.send_raw(&[0xff, 0xfe, 0x00, b'<', b'x', 0x00, 0x00]).await;
        noisy.send_raw(b"<LSX><Request recipient=\"EALS\"").await;
        noisy.send_raw(b"\0<LSX></LSX>\0not xml at all\0").await;

        // Garbage after the handshake: undecryptable and truncated ciphertext.
        let mut encrypted = Client::connect(port).await;
        encrypted.handshake("Encrypted").await;
        encrypted.send_raw(b"zz-not-hex\0abc\0deadbeef\0").await;
        encrypted.send_raw(&[0xc3, 0x28, 0x00]).await;
        // ... and it is still usable afterwards.
        assert_eq!(encrypted.game_info("2").await, "en_US");

        // A client that vanishes mid-message.
        let mut truncated = Client::connect(port).await;
        truncated.handshake("Truncated").await;
        truncated.send_raw(b"3f6a0c").await;
        drop(truncated);

        // A client that hammers an oversized frame is disconnected, alone.
        let mut flood = Client::connect(port).await;
        let _ = flood.frame().await;
        let _ = flood.stream.write_all(&vec![b'a'; 2 * 1024 * 1024]).await;
        let closed = flood.raw_frame().await;
        assert!(closed.is_none(), "the oversized frame should end that connection");

        assert_eq!(healthy.game_info("2").await, "en_US");
        assert_eq!(encrypted.game_info("3").await, "en_US");

        drop(noisy);
        drop(flood);
        wait_for_connections(&maxima, 2).await;

        // And a brand new client can still connect and talk.
        let mut fresh = Client::connect(port).await;
        fresh.handshake("Fresh").await;
        assert_eq!(fresh.game_info("2").await, "en_US");
    }

    #[tokio::test]
    async fn disconnecting_clients_are_forgotten() {
        let (maxima, port) = start().await;
        let mut first = Client::connect(port).await;
        let mut second = Client::connect(port).await;
        first.handshake("First").await;
        second.handshake("Second").await;
        wait_for_connections(&maxima, 2).await;

        drop(first);
        wait_for_connections(&maxima, 1).await;
        assert_eq!(second.game_info("2").await, "en_US");

        drop(second);
        wait_for_connections(&maxima, 0).await;
    }

    #[tokio::test]
    async fn unimplemented_requests_get_a_generic_error_with_their_id() {
        let (_maxima, port) = start().await;
        let mut client = Client::connect(port).await;
        client.handshake("TestGame").await;

        client
            .send_xml(r#"<LSX><Request recipient="EALS" id="77"><QueryAchievements version="" UserId="1"/></Request></LSX>"#)
            .await;

        let reply: LSX = quick_xml::de::from_str(&client.frame().await).unwrap();
        let LSXMessageType::Response(reply) = reply.value else {
            panic!("expected a response");
        };
        assert_eq!(reply.id, "77");
        match reply.value {
            LSXResponseType::ErrorSuccess(error) => {
                assert_ne!(error.attr_Code, 0);
                assert!(error.attr_Description.contains("QueryAchievements"));
            }
            other => panic!("unexpected response {:?}", other),
        }

        // The connection is still good.
        assert_eq!(client.game_info("78").await, "en_US");
    }

    #[tokio::test]
    async fn multiple_messages_in_one_write_are_all_answered() {
        let (_maxima, port) = start().await;
        let mut client = Client::connect(port).await;
        client.handshake("TestGame").await;

        let request = |id: &str| {
            quick_xml::se::to_string(&LSX {
                value: LSXMessageType::Request(LSXRequest {
                    recipient: "EALS".into(),
                    id: id.into(),
                    value: LSXRequestType::GetGameInfo(LSXGetGameInfo {
                        attr_GameInfoId: LSXGameInfoId::FreeTrial,
                        attr_version: String::new(),
                    }),
                }),
            })
            .unwrap()
        };

        let key = client.key.unwrap();
        let mut bytes = Vec::new();
        for id in ["10", "11", "12"] {
            bytes.extend_from_slice(simple_encrypt(request(id).as_bytes(), &key).as_bytes());
            bytes.push(0);
        }
        client.send_raw(&bytes).await;

        let mut seen = Vec::new();
        for _ in 0..3 {
            let reply: LSX = quick_xml::de::from_str(&client.frame().await).unwrap();
            let LSXMessageType::Response(reply) = reply.value else {
                panic!("expected a response");
            };
            seen.push(reply.id);
        }
        seen.sort();
        assert_eq!(seen, ["10", "11", "12"]);
    }

    #[tokio::test]
    async fn start_lsx_records_the_port_it_bound_and_defers_to_an_existing_server() {
        let maxima = test_maxima().await;
        maxima.lock().await.set_lsx_port(0);
        {
            let guard = maxima.lock().await;
            assert_eq!(guard.lsx_bound_port(), None);
            assert_eq!(guard.effective_lsx_port(), 0);
            guard.start_lsx(maxima.clone()).await.unwrap();
        }

        let port = maxima.lock().await.lsx_bound_port().expect("start_lsx should bind");
        assert_ne!(port, 0);
        assert_eq!(maxima.lock().await.effective_lsx_port(), port);

        let mut client = Client::connect(port).await;
        client.handshake("TestGame").await;
        assert_eq!(client.game_info("2").await, "en_US");

        // A second instance pointed at the same port must not bind.
        let other = test_maxima().await;
        other.lock().await.set_lsx_port(port);
        {
            let guard = other.lock().await;
            guard.start_lsx(other.clone()).await.unwrap();
        }
        let guard = other.lock().await;
        assert_eq!(guard.lsx_bound_port(), None);
        assert_eq!(guard.effective_lsx_port(), port);
    }

    #[tokio::test]
    async fn default_port_falls_back_to_a_free_one_when_taken() {
        // Hold a port the way another instance would hold 3216.
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = occupied.local_addr().unwrap().port();

        let maxima = test_maxima().await;
        {
            let mut guard = maxima.lock().await;
            guard.set_lsx_port(taken);
            guard.lsx_port_fixed = false;
            guard.start_lsx(maxima.clone()).await.unwrap();
        }
        let bound = maxima.lock().await.lsx_bound_port().expect("should bind a free port");
        assert_ne!(bound, taken);
        assert_eq!(maxima.lock().await.effective_lsx_port(), bound);
    }

    #[tokio::test]
    async fn all_game_info_reflects_the_client_not_a_hardcoded_title() {
        let (_maxima, port) = start().await;

        let mut client = Client::connect(port).await;
        client.handshake("SomeOtherGame").await;
        client
            .send(
                LSXRequestType::GetAllGameInfo(LSXGetAllGameInfo {
                    attr_version: String::new(),
                }),
                "5",
            )
            .await;
        let reply: LSX = quick_xml::de::from_str(&client.frame().await).unwrap();
        let LSXMessageType::Response(reply) = reply.value else {
            panic!("expected a response");
        };
        match reply.value {
            LSXResponseType::GetAllGameInfoResponse(info) => {
                // No game launched through Maxima: the challenge is all we know.
                assert_eq!(info.attr_DisplayName, "SomeOtherGame");
                assert_eq!(info.attr_InstalledVersion, "1.2.3.4");
                assert_eq!(info.attr_AvailableVersion, "1.2.3.4");
                assert_eq!(info.attr_EntitlementSource, "EA");
                assert!(!info.attr_FullGameReleaseDate.starts_with("2016"));
            }
            other => panic!("unexpected response {:?}", other),
        }

        // Before any challenge there is nothing to report, so nothing is made up.
        let mut early = Client::connect(port).await;
        let _ = early.frame().await;
        early
            .send(
                LSXRequestType::GetAllGameInfo(LSXGetAllGameInfo {
                    attr_version: String::new(),
                }),
                "6",
            )
            .await;
        let reply: LSX = quick_xml::de::from_str(&early.frame().await).unwrap();
        let LSXMessageType::Response(reply) = reply.value else {
            panic!("expected a response");
        };
        match reply.value {
            LSXResponseType::GetAllGameInfoResponse(info) => {
                assert_eq!(info.attr_DisplayName, "");
                assert_eq!(info.attr_InstalledVersion, "");
                assert_eq!(info.attr_AvailableVersion, "");
            }
            other => panic!("unexpected response {:?}", other),
        }
    }

    #[tokio::test]
    async fn bind_reports_the_port_the_os_picked() {
        let first = bind(0).await.unwrap();
        let second = bind(0).await.unwrap();
        assert_ne!(first.port(), 0);
        assert_ne!(first.port(), second.port());
        assert!(
            bind(first.port()).await.is_err(),
            "binding an occupied port is an error"
        );
    }
}
