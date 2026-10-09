use log::{info, warn};
use tokio::sync::broadcast;

use super::{
    backend::{BackendSelection, PresenceBackend, PresenceBackendKind},
    model::{BasicPresence, LockedPresenceStore, PresenceEvent, PresenceSink, PresenceUpdate},
};
use crate::{
    core::auth::storage::LockedAuthStorage,
    rtm::{
        client::{RtmClient, RtmDialect},
        RtmError,
    },
};

/// The presence front door `Maxima` owns. Callers don't know or care which
/// transport is behind it; before `login()` succeeds there is no backend and
/// every call fails with [`RtmError::NotLoggedIn`] (which all callers already
/// treat as non-fatal).
pub struct PresenceClient {
    auth: LockedAuthStorage,
    selection: BackendSelection,
    sink: PresenceSink,
    backend: Option<Box<dyn PresenceBackend>>,
}

impl PresenceClient {
    /// Backend chosen by `MAXIMA_PRESENCE_BACKEND` (default `legacy`).
    pub fn new(auth: LockedAuthStorage) -> Self {
        Self::with_selection(auth, BackendSelection::from_env())
    }

    pub fn with_selection(auth: LockedAuthStorage, selection: BackendSelection) -> Self {
        Self {
            auth,
            selection,
            sink: PresenceSink::new(),
            backend: None,
        }
    }

    pub fn selection(&self) -> BackendSelection {
        self.selection
    }

    /// The backend in use, once logged in.
    pub fn backend_kind(&self) -> Option<PresenceBackendKind> {
        self.backend.as_ref().map(|backend| backend.kind())
    }

    /// Friend presences, shared by every backend.
    pub fn presence_store(&self) -> &LockedPresenceStore {
        self.sink.store()
    }

    /// Presence-change notifications, shared by every backend.
    pub fn events(&self) -> broadcast::Receiver<PresenceEvent> {
        self.sink.subscribe()
    }

    fn build(&self, kind: PresenceBackendKind) -> Result<Box<dyn PresenceBackend>, RtmError> {
        match kind {
            PresenceBackendKind::Legacy => Ok(Box::new(RtmClient::with_sink(
                self.auth.clone(),
                RtmDialect::Legacy,
                self.sink.clone(),
            ))),
            PresenceBackendKind::Antelope => Ok(Box::new(RtmClient::with_sink(
                self.auth.clone(),
                RtmDialect::Antelope,
                self.sink.clone(),
            ))),
            #[cfg(feature = "presence-grpc")]
            PresenceBackendKind::Grpc => Ok(Box::new(super::grpc::GrpcPresenceBackend::new(
                self.auth.clone(),
                self.sink.clone(),
            ))),
            #[cfg(not(feature = "presence-grpc"))]
            PresenceBackendKind::Grpc => Err(RtmError::Presence(
                "built without the `presence-grpc` feature".to_owned(),
            )),
        }
    }

    /// Log in with the selected backend. With `auto`, a failing backend is
    /// abandoned for the next one in line.
    pub async fn login(&mut self) -> Result<(), RtmError> {
        if let Some(backend) = self.backend.as_mut() {
            return backend.login().await;
        }

        let attempts = self.selection.attempts(PresenceBackendKind::Grpc.is_compiled());
        let mut last_error = RtmError::Login;
        for (index, kind) in attempts.iter().enumerate() {
            let mut backend = match self.build(*kind) {
                Ok(backend) => backend,
                Err(err) => {
                    warn!("Presence backend '{}' unavailable: {}", kind.name(), err);
                    last_error = err;
                    continue;
                }
            };

            match backend.login().await {
                Ok(()) => {
                    info!("Presence backend: {}", kind.name());
                    self.backend = Some(backend);
                    return Ok(());
                }
                Err(err) => {
                    if index + 1 < attempts.len() {
                        warn!(
                            "Presence backend '{}' failed to log in ({}); trying '{}'",
                            kind.name(),
                            err,
                            attempts[index + 1].name()
                        );
                    }
                    last_error = err;
                }
            }
        }

        Err(last_error)
    }

    pub async fn subscribe(&mut self, players: &[String]) -> Result<(), RtmError> {
        self.backend_mut()?.subscribe(players).await
    }

    pub async fn update_presence(&mut self, update: &PresenceUpdate) -> Result<(), RtmError> {
        self.backend_mut()?.set_presence(update).await
    }

    /// Set presence from a single status line, as the pre-backend API did.
    pub async fn set_presence(
        &mut self,
        basic_presence: BasicPresence,
        status: &str,
        offer_id: &str,
    ) -> Result<(), RtmError> {
        self.update_presence(&PresenceUpdate {
            basic: basic_presence,
            offer_id: offer_id.to_owned(),
            rich_presence: status.to_owned(),
            ..Default::default()
        })
        .await
    }

    pub async fn heartbeat(&mut self) -> Result<(), RtmError> {
        self.backend_mut()?.heartbeat().await
    }

    fn backend_mut(&mut self) -> Result<&mut Box<dyn PresenceBackend>, RtmError> {
        self.backend.as_mut().ok_or(RtmError::NotLoggedIn)
    }
}
