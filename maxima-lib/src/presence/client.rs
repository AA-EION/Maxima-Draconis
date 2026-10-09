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

    /// Log in with the selected backend. With `auto`, a failing backend is
    /// abandoned for the next one in line.
    pub async fn login(&mut self) -> Result<(), RtmError> {
        if let Some(backend) = self.backend.as_mut() {
            return backend.login().await;
        }

        let attempts = self.selection.attempts(PresenceBackendKind::Grpc.is_compiled());
        let auth = self.auth.clone();
        let sink = self.sink.clone();
        self.login_through(&attempts, |kind| build_backend(kind, &auth, &sink))
            .await
    }

    async fn login_through(
        &mut self,
        attempts: &[PresenceBackendKind],
        build: impl Fn(PresenceBackendKind) -> Result<Box<dyn PresenceBackend>, RtmError>,
    ) -> Result<(), RtmError> {
        let mut last_error = RtmError::Login;
        for (index, kind) in attempts.iter().enumerate() {
            let mut backend = match build(*kind) {
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
                    if let Some(next) = attempts.get(index + 1) {
                        warn!(
                            "Presence backend '{}' failed to log in ({}); trying '{}'",
                            kind.name(),
                            err,
                            next.name()
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

fn build_backend(
    kind: PresenceBackendKind,
    auth: &LockedAuthStorage,
    sink: &PresenceSink,
) -> Result<Box<dyn PresenceBackend>, RtmError> {
    match kind {
        PresenceBackendKind::Legacy => Ok(Box::new(RtmClient::with_sink(
            auth.clone(),
            RtmDialect::Legacy,
            sink.clone(),
        ))),
        PresenceBackendKind::Antelope => Ok(Box::new(RtmClient::with_sink(
            auth.clone(),
            RtmDialect::Antelope,
            sink.clone(),
        ))),
        #[cfg(feature = "presence-grpc")]
        PresenceBackendKind::Grpc => Ok(Box::new(super::grpc::GrpcPresenceBackend::new(
            auth.clone(),
            sink.clone(),
        ))),
        #[cfg(not(feature = "presence-grpc"))]
        PresenceBackendKind::Grpc => Err(RtmError::Presence(
            "built without the `presence-grpc` feature".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use async_trait::async_trait;

    use super::*;
    use crate::{
        core::auth::storage::AuthStorage,
        presence::model::RichPresence,
    };

    struct Mock {
        kind: PresenceBackendKind,
        fail_login: bool,
        sink: PresenceSink,
        updates: Arc<StdMutex<Vec<PresenceUpdate>>>,
    }

    #[async_trait]
    impl PresenceBackend for Mock {
        fn kind(&self) -> PresenceBackendKind {
            self.kind
        }

        fn sink(&self) -> &PresenceSink {
            &self.sink
        }

        async fn login(&mut self) -> Result<(), RtmError> {
            if self.fail_login {
                Err(RtmError::Presence(format!("{} down", self.kind.name())))
            } else {
                self.sink
                    .publish(
                        "friend".into(),
                        RichPresence::new(BasicPresence::Online, "hi".into(), None),
                    )
                    .await;
                Ok(())
            }
        }

        async fn subscribe(&mut self, _friend_ids: &[String]) -> Result<(), RtmError> {
            Ok(())
        }

        async fn set_presence(&mut self, update: &PresenceUpdate) -> Result<(), RtmError> {
            self.updates.lock().unwrap().push(update.clone());
            Ok(())
        }

        async fn heartbeat(&mut self) -> Result<(), RtmError> {
            Ok(())
        }
    }

    type Updates = Arc<StdMutex<Vec<PresenceUpdate>>>;

    fn client() -> PresenceClient {
        PresenceClient::with_selection(
            AuthStorage::from_token("test"),
            BackendSelection::Auto,
        )
    }

    fn mock_builder(
        sink: &PresenceSink,
        updates: &Updates,
        failing: &'static [PresenceBackendKind],
    ) -> impl Fn(PresenceBackendKind) -> Result<Box<dyn PresenceBackend>, RtmError> {
        let sink = sink.clone();
        let updates = updates.clone();
        move |kind| {
            Ok(Box::new(Mock {
                kind,
                fail_login: failing.contains(&kind),
                sink: sink.clone(),
                updates: updates.clone(),
            }) as Box<dyn PresenceBackend>)
        }
    }

    #[tokio::test]
    async fn everything_fails_softly_before_login() {
        let mut client = client();
        assert!(matches!(
            client.heartbeat().await,
            Err(RtmError::NotLoggedIn)
        ));
        assert!(matches!(
            client.subscribe(&["1".into()]).await,
            Err(RtmError::NotLoggedIn)
        ));
        assert!(matches!(
            client.set_presence(BasicPresence::Online, "", "").await,
            Err(RtmError::NotLoggedIn)
        ));
        assert_eq!(client.backend_kind(), None);
    }

    #[tokio::test]
    async fn auto_falls_back_when_the_first_backend_fails_to_log_in() {
        use PresenceBackendKind::*;
        let mut client = client();
        let updates = Updates::default();
        let build = mock_builder(&client.sink, &updates, &[Grpc]);

        client
            .login_through(&[Grpc, Legacy], build)
            .await
            .unwrap();
        assert_eq!(client.backend_kind(), Some(Legacy));
    }

    #[tokio::test]
    async fn a_working_first_choice_is_kept() {
        use PresenceBackendKind::*;
        let mut client = client();
        let updates = Updates::default();
        let build = mock_builder(&client.sink, &updates, &[]);

        client
            .login_through(&[Grpc, Legacy], build)
            .await
            .unwrap();
        assert_eq!(client.backend_kind(), Some(Grpc));
    }

    #[tokio::test]
    async fn login_reports_the_last_error_when_every_backend_fails() {
        use PresenceBackendKind::*;
        let mut client = client();
        let updates = Updates::default();
        let build = mock_builder(&client.sink, &updates, &[Grpc, Legacy]);

        let err = client
            .login_through(&[Grpc, Legacy], build)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("legacy down"), "{}", err);
        assert_eq!(client.backend_kind(), None);
    }

    #[tokio::test]
    async fn a_backend_that_cannot_be_built_is_skipped() {
        use PresenceBackendKind::*;
        let mut client = client();
        let updates = Updates::default();
        let sink = client.sink.clone();
        let ok = mock_builder(&sink, &updates, &[]);

        client
            .login_through(&[Grpc, Legacy], |kind| match kind {
                Grpc => Err(RtmError::Presence("not compiled".into())),
                other => ok(other),
            })
            .await
            .unwrap();
        assert_eq!(client.backend_kind(), Some(Legacy));
    }

    #[tokio::test]
    async fn presence_flows_through_whichever_backend_won() {
        use PresenceBackendKind::*;
        let mut client = client();
        let mut events = client.events();
        let updates = Updates::default();
        let build = mock_builder(&client.sink, &updates, &[]);
        client.login_through(&[Legacy], build).await.unwrap();

        client
            .set_presence(BasicPresence::Online, "In the menus", "Origin.OFR.50.0000001")
            .await
            .unwrap();
        let sent = updates.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].rich_presence, "In the menus");
        assert_eq!(sent[0].offer_id, "Origin.OFR.50.0000001");

        // The backend's friend update reached the shared store and stream.
        assert!(client.presence_store().lock().await.get("friend").is_some());
        assert!(matches!(
            events.recv().await,
            Ok(crate::presence::PresenceEvent::Friend { .. })
        ));
    }
}
