//! The RPC envelope types. They serialize to the exact newline-JSON wire the
//! server has always spoken, so the SwiftUI app (which parses that JSON
//! directly) is unaffected by the move to typed messages:
//!
//!   request       {"id":N,"cmd":"launch","slug":"…",…}
//!   response ok    {"id":N,"ok":true,"games":[…]}
//!   response err   {"id":N,"ok":false,"error":"…"}
//!   notification   {"event":"presence","id":"…",…}

use serde::{Deserialize, Serialize};
use serde_json::Value;

fn default_true() -> bool {
    true
}

/// A client → server request. `cmd` is the tag; per-command fields sit
/// alongside it. Wrapped in [`RequestEnvelope`] to carry the correlation id.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    ListGames,
    Friends,
    Status,
    Shutdown,
    GameDetails {
        slug: String,
    },
    Launch {
        slug: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        exe_override: Option<String>,
        #[serde(default = "default_true")]
        cloud_saves: bool,
    },
    Install {
        slug: String,
        #[serde(default)]
        path: Option<String>,
    },
    LocateGame {
        path: String,
    },
    CloudSync {
        slug: String,
        #[serde(default)]
        write: bool,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RequestEnvelope {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

/// A server → client response, matched to a request by `id`. `data` holds the
/// per-command payload fields (games / friends / details / status / …).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResponseEnvelope {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(flatten)]
    pub data: Value,
}

impl ResponseEnvelope {
    pub fn ok(id: u64, data: Value) -> Self {
        Self { id, ok: true, error: None, data }
    }
    pub fn err(id: u64, error: impl Into<String>) -> Self {
        Self { id, ok: false, error: Some(error.into()), data: Value::Null }
    }
    /// Extract a named field from `data` and deserialize it.
    pub fn field<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Option<T> {
        self.data.get(key).cloned().and_then(|v| serde_json::from_value(v).ok())
    }
}

/// A server → client broadcast event (no id), pushed to every client.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Notification {
    Ready {
        persona: String,
    },
    Presence {
        id: String,
        basic: String,
        status: String,
        #[serde(default)]
        game: Option<String>,
    },
    InstallProgress {
        slug: String,
        percent: f64,
    },
    InstallDone {
        #[serde(default)]
        slug: Option<String>,
    },
    InstallError {
        #[serde(default)]
        slug: Option<String>,
        message: String,
    },
    GameStarted {
        slug: String,
    },
    GameStopped,
    DownloadQueue {
        #[serde(default)]
        current: Option<String>,
        #[serde(default)]
        queued: Vec<String>,
    },
}

/// Anything the server sends: a matched response, or a broadcast event.
/// `id` distinguishes them (responses have it, notifications don't).
#[derive(Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Response(ResponseEnvelope),
    Notification(Notification),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_wire_shape() {
        let env = RequestEnvelope {
            id: 7,
            request: Request::Launch {
                slug: "titanfall-2".into(),
                args: vec!["-northstar".into()],
                exe_override: None,
                cloud_saves: true,
            },
        };
        let s = serde_json::to_string(&env).unwrap();
        // id + flattened tagged command, matching the historical wire.
        assert!(s.contains("\"id\":7"));
        assert!(s.contains("\"cmd\":\"launch\""));
        assert!(s.contains("\"slug\":\"titanfall-2\""));
    }

    #[test]
    fn notification_kebab_tags() {
        let n = Notification::GameStarted { slug: "x".into() };
        assert_eq!(serde_json::to_string(&n).unwrap(), r#"{"event":"game-started","slug":"x"}"#);
        let n = Notification::InstallProgress { slug: "x".into(), percent: 12.5 };
        assert!(serde_json::to_string(&n).unwrap().contains("\"event\":\"install-progress\""));
    }

    #[test]
    fn server_message_discriminates_by_id() {
        // Response (has id) vs notification (has event, no id).
        let resp = r#"{"id":1,"ok":true,"games":[]}"#;
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(resp).unwrap(),
            ServerMessage::Response(_)
        ));
        let note = r#"{"event":"ready","persona":"Me"}"#;
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(note).unwrap(),
            ServerMessage::Notification(Notification::Ready { .. })
        ));
    }
}
