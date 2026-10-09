use std::{
    collections::HashMap,
    error::Error,
    io::{self, ErrorKind},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use super::proto::{communication_v1, Communication, CommunicationV1};
use super::RtmError;
use log::{debug, error, warn};
use prost::{
    bytes::{Buf, BufMut, BytesMut},
    Message,
};
use rustls::{ClientConfig, OwnedTrustAnchor};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, oneshot},
    time,
};
use tokio_rustls::TlsConnector;
use webpki_roots::TLS_SERVER_ROOTS;

// TnT as far as I've heard means "Tools and Technology"
pub const RTM_DOMAIN: &str = "rtm.tnt-ea.com";
pub const RTM_TCP_HOST: &str = "rtm.tnt-ea.com:9000";

// We don't use this, but it exists. EA Desktop natively connects to the TCP host,
// and connects to the WS host from the javascript frontend
pub const RTM_WS_HOST: &str = "wss://rtm.tnt-ea.com:8095/websocket";

/// Upper bound for a single incoming frame; real RTM frames are a few KiB.
const MAX_FRAME_SIZE: i32 = 16 * 1024 * 1024;

/// Reconnects back off exponentially up to this cap.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
/// A connection that lived this long counts as healthy and resets the backoff.
const HEALTHY_CONNECTION: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Enqueueing a request and waiting for its response are both bounded so a
/// dead RTM connection can never wedge a caller (who may hold the global
/// Maxima lock).
const ENQUEUE_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) fn next_reconnect_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_RECONNECT_DELAY)
}

enum StreamEnd {
    /// The server went away; connect again.
    Disconnected,
    /// Every sender is gone; nobody is left to serve.
    Shutdown,
}

pub struct RtmRequest {
    id: String,
    payload: communication_v1::Body,
    response_tx: Option<oneshot::Sender<Communication>>,
}

pub struct RtmConnectionManager {
    request_tx: mpsc::Sender<RtmRequest>,
    request_index: u32,
}

impl RtmConnectionManager {
    pub fn new(
        reconnect_delay: Duration,
        update_presence_tx: mpsc::Sender<communication_v1::Body>,
    ) -> RtmConnectionManager {
        let (request_tx, request_rx) = mpsc::channel(32);

        tokio::spawn(async move {
            RtmConnectionManager::run(reconnect_delay, request_rx, update_presence_tx).await;
        });

        Self {
            request_tx: request_tx.clone(),
            request_index: 0,
        }
    }

    async fn run(
        reconnect_delay: Duration,
        mut request_rx: mpsc::Receiver<RtmRequest>,
        mut update_presence_tx: mpsc::Sender<communication_v1::Body>,
    ) {
        let mut delay = reconnect_delay;
        loop {
            let started = Instant::now();
            match time::timeout(CONNECT_TIMEOUT, TcpStream::connect(RTM_TCP_HOST)).await {
                Ok(Ok(stream)) => {
                    match RtmConnectionManager::handle_stream(
                        stream,
                        &mut request_rx,
                        &mut update_presence_tx,
                    )
                    .await
                    {
                        Ok(StreamEnd::Shutdown) => return,
                        Ok(StreamEnd::Disconnected) => {}
                        Err(e) => warn!("RTM stream error: {}", e),
                    }
                }
                Ok(Err(e)) => warn!("Failed to connect to RTM: {}", e),
                Err(_) => warn!("Timed out connecting to RTM"),
            }

            if started.elapsed() >= HEALTHY_CONNECTION {
                delay = reconnect_delay;
            }
            debug!("Reconnecting to RTM in {:?}", delay);
            time::sleep(delay).await;
            delay = next_reconnect_delay(delay);
        }
    }

    async fn handle_stream(
        stream: TcpStream,
        request_rx: &mut mpsc::Receiver<RtmRequest>,
        update_presence_tx: &mut mpsc::Sender<communication_v1::Body>,
    ) -> Result<StreamEnd, Box<dyn Error>> {
        let anchors = TLS_SERVER_ROOTS.0.iter().map(|ta| {
            OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        });

        let mut store = rustls::RootCertStore::empty();
        store.add_server_trust_anchors(anchors);

        let config = ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(store)
            .with_no_client_auth();

        let connector = TlsConnector::from(Arc::new(config));

        let domain = rustls::ServerName::try_from(RTM_DOMAIN)?;
        let mut tls_stream = time::timeout(CONNECT_TIMEOUT, connector.connect(domain, stream))
            .await
            .map_err(|_| RtmError::Timeout)??;

        let mut pending_responses: HashMap<String, oneshot::Sender<Communication>> = HashMap::new();

        let mut expected_size: i32 = -1;
        let mut bytes = BytesMut::with_capacity(1024 * 4);

        loop {
            tokio::select! {
                size = tls_stream.read_buf(&mut bytes) => {
                    match size {
                        Ok(0) => {
                            warn!("RTM connection closed");
                            break;
                        },
                        Ok(_) => {
                            loop {
                                if expected_size == -1 {
                                    if bytes.len() < 4 {
                                        break;
                                    }

                                    expected_size = bytes.get_i32();

                                    // A bogus length prefix would otherwise make us
                                    // buffer unbounded data (or wrap to a huge usize).
                                    if !(0..=MAX_FRAME_SIZE).contains(&expected_size) {
                                        error!("RTM frame size {} is out of range, closing connection", expected_size);
                                        return Err(Box::new(RtmError::Io(io::Error::new(
                                            ErrorKind::InvalidData,
                                            "RTM frame size out of range",
                                        ))));
                                    }
                                }

                                let frame_len = expected_size as usize;
                                if bytes.len() < frame_len {
                                    break;
                                }

                                let buf = bytes.split_to(frame_len).freeze();
                                expected_size = -1;

                                let msg = Communication::decode(buf)?;
                                let Some(v1) = msg.v1.as_ref() else {
                                    warn!("Ignoring RTM message without a body");
                                    continue;
                                };
                                let id = v1.request_id.clone();

                                if let Some(tx) = pending_responses.remove(&id) {
                                    // The requester may have given up waiting; that's fine.
                                    let _ = tx.send(msg);
                                } else if id.is_empty() {
                                    if let Some(body) = &v1.body {
                                        update_presence_tx.send(body.clone()).await?;
                                    }
                                }
                            }
                        },
                        Err(e) => {
                            error!("Failed to read from RTM socket: {}", e);
                            break;
                        },
                    }
                },
                request = request_rx.recv(), if expected_size == -1 => {
                    if let Some(request) = request {
                        let communication = Communication {
                            v1: Some(CommunicationV1 {
                                request_id: request.id.to_owned(),
                                body: Some(request.payload),
                            }),
                        };

                        let mut buf = BytesMut::new();
                        buf.put_i32(communication.encoded_len() as i32);
                        communication.encode(&mut buf)?;

                        let frozen = buf.freeze();
                        tls_stream.write_all(frozen.chunk()).await?;

                        if let Some(response_tx) = request.response_tx {
                            pending_responses.insert(request.id, response_tx);
                        }
                    } else {
                        return Ok(StreamEnd::Shutdown);
                    }
                },
            }
        }

        Ok(StreamEnd::Disconnected)
    }

    async fn enqueue(&self, request: RtmRequest) -> Result<(), RtmError> {
        time::timeout(ENQUEUE_TIMEOUT, self.request_tx.send(request))
            .await
            .map_err(|_| RtmError::Timeout)??;
        Ok(())
    }

    pub async fn send_request(
        &mut self,
        message: communication_v1::Body,
    ) -> Result<communication_v1::Body, RtmError> {
        let (response_tx, response_rx) = oneshot::channel();
        let request_id = self.get_new_request_id();

        self.enqueue(RtmRequest {
            id: request_id,
            payload: message,
            response_tx: Some(response_tx),
        })
        .await?;

        match time::timeout(RESPONSE_TIMEOUT, response_rx).await {
            Err(_) => Err(RtmError::Timeout),
            Ok(Ok(response)) => Ok(response
                .v1
                .ok_or(RtmError::NoBody)?
                .body
                .ok_or(RtmError::NoBody)?),
            Ok(Err(_)) => Err(RtmError::Io(io::Error::new(
                ErrorKind::Other,
                "Failed to receive response",
            ))),
        }
    }

    pub async fn send_and_forget_request(
        &mut self,
        message: communication_v1::Body,
    ) -> Result<(), RtmError> {
        let request_id = self.get_new_request_id();

        self.enqueue(RtmRequest {
            id: request_id,
            payload: message,
            response_tx: None,
        })
        .await
    }

    fn get_new_request_id(&mut self) -> String {
        let secs_since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards..?")
            .as_secs();
        let request_id = format!(
            "c-{}-{}-{}",
            self.request_index, secs_since_epoch, secs_since_epoch
        );

        self.request_index += 1;

        request_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_delay_doubles_up_to_the_cap() {
        let mut delay = Duration::from_millis(50);
        let mut seen = vec![delay];
        for _ in 0..20 {
            delay = next_reconnect_delay(delay);
            seen.push(delay);
        }
        assert_eq!(seen[1], Duration::from_millis(100));
        assert_eq!(seen[2], Duration::from_millis(200));
        assert!(seen.windows(2).all(|w| w[1] >= w[0]));
        assert_eq!(*seen.last().unwrap(), MAX_RECONNECT_DELAY);
    }
}
