use std::{sync::Arc, time::Duration};

use derive_builder::Builder;
use derive_getters::Getters;
use moka::sync::Cache;
use tokio::sync::{broadcast, Mutex};

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum BasicPresence {
    #[default]
    Unknown,
    Offline,
    /// Doesn't work
    Dnd,
    Away,
    Online,
}

/// What we know about one friend. `game` is the offer id of the game they are
/// in, `status` the human-readable rich-presence line.
#[derive(Clone, Builder, Getters, Debug, Default, PartialEq, Eq)]
pub struct RichPresence {
    basic: BasicPresence,
    status: String,
    game: Option<String>,
    #[builder(default)]
    game_title: Option<String>,
    #[builder(default)]
    multiplayer_id: Option<String>,
    #[builder(default)]
    game_presence: Option<String>,
    #[builder(default)]
    group_id: Option<String>,
    #[builder(default)]
    group_name: Option<String>,
    #[builder(default)]
    joinable: bool,
    #[builder(default)]
    joinable_invite_only: bool,
}

impl RichPresence {
    pub fn new(basic: BasicPresence, status: String, game: Option<String>) -> Self {
        Self {
            basic,
            status,
            game,
            ..Default::default()
        }
    }

    pub fn with_game_title(mut self, title: Option<String>) -> Self {
        self.game_title = title;
        self
    }

    pub fn with_multiplayer_id(mut self, id: Option<String>) -> Self {
        self.multiplayer_id = id;
        self
    }

    pub fn with_game_presence(mut self, presence: Option<String>) -> Self {
        self.game_presence = presence;
        self
    }

    pub fn with_group(mut self, id: Option<String>, name: Option<String>) -> Self {
        self.group_id = id;
        self.group_name = name;
        self
    }

    pub fn with_joinable(mut self, joinable: bool, invite_only: bool) -> Self {
        self.joinable = joinable;
        self.joinable_invite_only = invite_only;
        self
    }

    /// A friend we know nothing about (not in the store).
    pub fn offline() -> Self {
        Self::new(BasicPresence::Offline, String::new(), None)
    }
}

/// An outgoing presence change for the local user.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PresenceUpdate {
    pub basic: BasicPresence,
    pub offer_id: String,
    pub game_title: String,
    /// The game's own rich-presence line, e.g. "In the menus".
    pub rich_presence: String,
    /// The game's opaque `GamePresence` blob, if it supplied one.
    pub game_presence: Option<String>,
    /// The game's session / group id, if it supplied one.
    pub session_id: Option<String>,
    pub joinable: bool,
    pub joinable_invite_only: bool,
}

impl PresenceUpdate {
    /// The single status string the legacy RTM client has always sent:
    /// `"<title>: <rich presence>"`, or just the rich presence when no title
    /// is known.
    pub fn legacy_status(&self) -> String {
        if self.game_title.is_empty() {
            self.rich_presence.clone()
        } else {
            format!("{}: {}", self.game_title, self.rich_presence)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresenceEvent {
    /// A friend's presence changed (or was seen for the first time).
    Friend { id: String, presence: RichPresence },
    /// The backend hit a problem it is recovering from by itself.
    BackendError(String),
}

pub type LockedPresenceStore = Arc<Mutex<Cache<String, RichPresence>>>;

const STORE_CAPACITY: u64 = 256;
const STORE_TTL: Duration = Duration::from_secs(60 * 5);
const EVENT_BACKLOG: usize = 256;

/// Where backends put what they learn. Writes the shared store (what the
/// server's tick loop, the UI and the LSX handlers read) and announces
/// changes on the event stream.
#[derive(Clone)]
pub struct PresenceSink {
    store: LockedPresenceStore,
    events: broadcast::Sender<PresenceEvent>,
}

impl Default for PresenceSink {
    fn default() -> Self {
        Self::new()
    }
}

impl PresenceSink {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(EVENT_BACKLOG);
        Self {
            store: Arc::new(Mutex::new(
                Cache::builder()
                    .max_capacity(STORE_CAPACITY)
                    .time_to_idle(Duration::from_secs_f64(3.154e+7f64)) // 1 year
                    .time_to_live(STORE_TTL)
                    .build(),
            )),
            events,
        }
    }

    pub fn store(&self) -> &LockedPresenceStore {
        &self.store
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PresenceEvent> {
        self.events.subscribe()
    }

    /// Record a friend's presence. Returns whether it differed from what we
    /// already had; only real changes are announced.
    pub async fn publish(&self, id: String, presence: RichPresence) -> bool {
        let changed = {
            let store = self.store.lock().await;
            let changed = store.get(&id).as_ref() != Some(&presence);
            store.insert(id.clone(), presence.clone());
            changed
        };
        if changed {
            // No receivers is fine; the store is the source of truth.
            let _ = self.events.send(PresenceEvent::Friend { id, presence });
        }
        changed
    }

    /// Re-insert a presence we already announced so the store's TTL doesn't
    /// age out a friend whose (event-driven) presence simply hasn't changed.
    pub async fn refresh(&self, id: String, presence: RichPresence) {
        self.store.lock().await.insert(id, presence);
    }

    pub fn report_error(&self, message: impl Into<String>) {
        let _ = self.events.send(PresenceEvent::BackendError(message.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn online(status: &str) -> RichPresence {
        RichPresence::new(BasicPresence::Online, status.to_owned(), None)
    }

    #[tokio::test]
    async fn publish_stores_and_announces_only_changes() {
        let sink = PresenceSink::new();
        let mut rx = sink.subscribe();

        assert!(sink.publish("1".into(), online("a")).await);
        assert!(!sink.publish("1".into(), online("a")).await);
        assert!(sink.publish("1".into(), online("b")).await);

        let first = rx.recv().await.unwrap();
        assert_eq!(
            first,
            PresenceEvent::Friend {
                id: "1".into(),
                presence: online("a")
            }
        );
        let second = rx.recv().await.unwrap();
        assert_eq!(
            second,
            PresenceEvent::Friend {
                id: "1".into(),
                presence: online("b")
            }
        );
        assert!(rx.try_recv().is_err());

        let store = sink.store().lock().await;
        assert_eq!(store.get("1"), Some(online("b")));
    }

    #[tokio::test]
    async fn refresh_does_not_announce() {
        let sink = PresenceSink::new();
        let mut rx = sink.subscribe();
        sink.refresh("7".into(), online("x")).await;
        assert!(rx.try_recv().is_err());
        assert_eq!(sink.store().lock().await.get("7"), Some(online("x")));
    }

    #[test]
    fn legacy_status_matches_the_historic_format() {
        let with_title = PresenceUpdate {
            game_title: "Example Game".into(),
            rich_presence: "In the menus".into(),
            ..Default::default()
        };
        assert_eq!(with_title.legacy_status(), "Example Game: In the menus");

        let bare = PresenceUpdate {
            rich_presence: "Online".into(),
            ..Default::default()
        };
        assert_eq!(bare.legacy_status(), "Online");
        assert_eq!(PresenceUpdate::default().legacy_status(), "");
    }

    #[test]
    fn builder_defaults_the_extended_fields() {
        let p = RichPresenceBuilder::default()
            .basic(BasicPresence::Offline)
            .status(String::new())
            .game(None)
            .build()
            .unwrap();
        assert_eq!(p, RichPresence::offline());
        assert!(!p.joinable());
    }
}
