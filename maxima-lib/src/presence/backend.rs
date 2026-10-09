use async_trait::async_trait;
use log::warn;

use super::model::{PresenceSink, PresenceUpdate, RichPresence};
use crate::rtm::RtmError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresenceBackendKind {
    /// The original RTM client.
    Legacy,
    /// RTM with upstream PR 70's "Antelope" payload dialect.
    Antelope,
    /// EA's social gRPC presence service (cargo feature `presence-grpc`).
    Grpc,
}

impl PresenceBackendKind {
    pub fn name(self) -> &'static str {
        match self {
            PresenceBackendKind::Legacy => "legacy",
            PresenceBackendKind::Antelope => "antelope",
            PresenceBackendKind::Grpc => "grpc",
        }
    }

    /// Whether this build contains the backend.
    pub fn is_compiled(self) -> bool {
        match self {
            PresenceBackendKind::Grpc => cfg!(feature = "presence-grpc"),
            _ => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendSelection {
    Fixed(PresenceBackendKind),
    /// Try the newest backend this build has, fall back to legacy on failure.
    Auto,
}

impl Default for BackendSelection {
    fn default() -> Self {
        BackendSelection::Fixed(PresenceBackendKind::Legacy)
    }
}

impl BackendSelection {
    pub const ENV: &'static str = "MAXIMA_PRESENCE_BACKEND";

    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "legacy" | "rtm" => Ok(BackendSelection::Fixed(PresenceBackendKind::Legacy)),
            "antelope" | "rtm-antelope" => Ok(BackendSelection::Fixed(PresenceBackendKind::Antelope)),
            "grpc" | "social" => Ok(BackendSelection::Fixed(PresenceBackendKind::Grpc)),
            "auto" => Ok(BackendSelection::Auto),
            other => Err(format!(
                "unknown presence backend `{}` (expected legacy, antelope, grpc or auto)",
                other
            )),
        }
    }

    /// Reads `MAXIMA_PRESENCE_BACKEND`; unset or invalid means `legacy`.
    pub fn from_env() -> Self {
        match std::env::var(Self::ENV) {
            Ok(value) => Self::parse(&value).unwrap_or_else(|err| {
                warn!("{}: {}; using legacy", Self::ENV, err);
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// The backends to try, in order, given what this build contains. A
    /// backend that was asked for but isn't compiled in degrades to legacy.
    pub fn attempts(self, grpc_compiled: bool) -> Vec<PresenceBackendKind> {
        match self {
            BackendSelection::Fixed(PresenceBackendKind::Grpc) if !grpc_compiled => {
                warn!(
                    "{}=grpc requested but this build lacks the `presence-grpc` feature; using legacy",
                    Self::ENV
                );
                vec![PresenceBackendKind::Legacy]
            }
            BackendSelection::Fixed(kind) => vec![kind],
            BackendSelection::Auto if grpc_compiled => {
                vec![PresenceBackendKind::Grpc, PresenceBackendKind::Legacy]
            }
            BackendSelection::Auto => vec![PresenceBackendKind::Legacy],
        }
    }
}

/// One way of getting friends' presence in and the local user's presence out.
///
/// A backend writes everything it learns into its [`PresenceSink`]; the shared
/// store and event stream live there so they survive switching backends.
#[async_trait]
pub trait PresenceBackend: Send + Sync {
    fn kind(&self) -> PresenceBackendKind;

    fn sink(&self) -> &PresenceSink;

    /// Authenticate and open the backend's session.
    async fn login(&mut self) -> Result<(), RtmError>;

    /// Start receiving the given friends' presence. Backends that follow the
    /// whole friends list server-side may ignore the ids.
    async fn subscribe(&mut self, friend_ids: &[String]) -> Result<(), RtmError>;

    async fn set_presence(&mut self, update: &PresenceUpdate) -> Result<(), RtmError>;

    /// Keep the session alive; a no-op for transports that do it themselves.
    async fn heartbeat(&mut self) -> Result<(), RtmError>;

    /// The last presence seen for one friend.
    async fn query_presence(&self, friend_id: &str) -> Option<RichPresence> {
        self.sink().store().lock().await.get(friend_id)
    }

    /// Every friend presence currently known.
    async fn friends(&self) -> Vec<(String, RichPresence)> {
        self.sink()
            .store()
            .lock()
            .await
            .iter()
            .map(|(id, presence)| (id.as_ref().clone(), presence))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_documented_value() {
        use PresenceBackendKind::*;
        assert_eq!(BackendSelection::parse("legacy"), Ok(BackendSelection::Fixed(Legacy)));
        assert_eq!(BackendSelection::parse(" LEGACY "), Ok(BackendSelection::Fixed(Legacy)));
        assert_eq!(BackendSelection::parse(""), Ok(BackendSelection::Fixed(Legacy)));
        assert_eq!(BackendSelection::parse("antelope"), Ok(BackendSelection::Fixed(Antelope)));
        assert_eq!(BackendSelection::parse("grpc"), Ok(BackendSelection::Fixed(Grpc)));
        assert_eq!(BackendSelection::parse("auto"), Ok(BackendSelection::Auto));
        assert!(BackendSelection::parse("carrier-pigeon").is_err());
    }

    #[test]
    fn default_is_legacy() {
        assert_eq!(
            BackendSelection::default(),
            BackendSelection::Fixed(PresenceBackendKind::Legacy)
        );
    }

    #[test]
    fn attempts_respect_what_is_compiled() {
        use PresenceBackendKind::*;
        assert_eq!(BackendSelection::Fixed(Legacy).attempts(true), vec![Legacy]);
        assert_eq!(BackendSelection::Fixed(Antelope).attempts(false), vec![Antelope]);
        assert_eq!(BackendSelection::Fixed(Grpc).attempts(true), vec![Grpc]);
        assert_eq!(BackendSelection::Fixed(Grpc).attempts(false), vec![Legacy]);
        assert_eq!(BackendSelection::Auto.attempts(true), vec![Grpc, Legacy]);
        assert_eq!(BackendSelection::Auto.attempts(false), vec![Legacy]);
    }
}
