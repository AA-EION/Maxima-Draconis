//! The RPC envelope types. They serialize to the exact newline-JSON wire the
//! server has always spoken, so the SwiftUI app (which parses that JSON
//! directly) is unaffected by the move to typed messages:
//!
//!   request       {"id":N,"cmd":"launch","slug":"…",…}
//!   response ok    {"id":N,"ok":true,"games":[…]}
//!   response err   {"id":N,"ok":false,"error":"…","kind":"login-pending"}
//!   notification   {"event":"presence","id":"…",…}
//!
//! The first request on every connection must be `hello`, carrying the token
//! from the server's `instance.json` (see [`crate::instance`]); anything else
//! is answered with an `unauthorized` error and the connection is closed.

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
    Hello {
        token: String,
        /// Free-form client name for the server log, e.g. `maxima-cli/0.14.0`.
        #[serde(default)]
        client: String,
        proto: u32,
    },
    ListGames,
    Friends,
    Status,
    Shutdown,
    /// The signed-in user (persona + id + avatar url).
    WhoAmI,
    GameDetails {
        slug: String,
    },
    /// Lazily fetch a game's image URLs (hero / logo / background).
    GameImages {
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
        #[serde(default)]
        build_id: Option<String>,
        /// Files (relative to the install path) to force-replace before the
        /// install runs — deleted so the downloader re-fetches them. Not
        /// game-specific: any file of any title (the Steam-CEG fix is just
        /// one caller).
        #[serde(default)]
        replace_files: Vec<String>,
        /// Restrict the install to ONLY `replace_files` (surgical refresh).
        #[serde(default)]
        only_listed_files: bool,
    },
    LocateGame {
        path: String,
    },
    CloudSync {
        slug: String,
        #[serde(default)]
        write: bool,
    },
    /// Size-verify a game's files against the build manifest; `repair`
    /// re-downloads the broken ones.
    Verify {
        slug: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        repair: bool,
    },
    /// Download a single named file from a game's build manifest.
    DownloadFile {
        slug: String,
        #[serde(default)]
        build_id: Option<String>,
        file: String,
    },
    /// Read-only bottle / prefix / game-dir readout (creates nothing).
    BottleInfo {
        slug: String,
    },
    /// Register Maxima's URL protocol handlers with the host OS.
    RegisterProtocols,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RequestEnvelope {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

/// Why a request failed, so clients can react without matching on messages.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorKind {
    /// No valid `hello` on this connection.
    Unauthorized,
    /// The server is up but still waiting for the EA login to finish.
    LoginPending,
    /// The client speaks a protocol version the server doesn't.
    IncompatibleVersion,
    /// The request couldn't be parsed or its arguments are wrong.
    Invalid,
    /// Something is already running that this request would conflict with.
    Busy,
    #[serde(other)]
    Internal,
}

/// A server → client response, matched to a request by `id`. `data` holds the
/// per-command payload fields (games / friends / details / status / …).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResponseEnvelope {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ErrorKind>,
    #[serde(flatten)]
    pub data: Value,
}

impl ResponseEnvelope {
    pub fn ok(id: u64, data: Value) -> Self {
        Self { id, ok: true, error: None, kind: None, data }
    }
    pub fn err(id: u64, error: impl Into<String>) -> Self {
        Self::fail(id, ErrorKind::Internal, error)
    }
    pub fn fail(id: u64, kind: ErrorKind, error: impl Into<String>) -> Self {
        Self { id, ok: false, error: Some(error.into()), kind: Some(kind), data: Value::Null }
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
    /// The session is logged in. Sent after `hello` when it already is, and
    /// to every client when a pending login completes.
    Ready {
        persona: String,
    },
    /// Sent after `hello` while the server waits for the EA login.
    LoginRequired,
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
    VerifyProgress {
        slug: String,
        files_checked: u64,
        total_files: u64,
    },
    VerifyDone {
        slug: String,
        ok: u64,
        broken: u64,
        repaired: bool,
    },
    VerifyError {
        slug: String,
        message: String,
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
    fn hello_wire_shape() {
        let env = RequestEnvelope {
            id: 1,
            request: Request::Hello { token: "t".into(), client: "c".into(), proto: 2 },
        };
        assert_eq!(
            serde_json::to_string(&env).unwrap(),
            r#"{"id":1,"cmd":"hello","token":"t","client":"c","proto":2}"#
        );
    }

    #[test]
    fn unknown_error_kind_is_internal() {
        let kind: ErrorKind = serde_json::from_str(r#""something-new""#).unwrap();
        assert_eq!(kind, ErrorKind::Internal);
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
        let err = r#"{"id":2,"ok":false,"error":"x","kind":"login-pending"}"#;
        match serde_json::from_str::<ServerMessage>(err).unwrap() {
            ServerMessage::Response(r) => assert_eq!(r.kind, Some(ErrorKind::LoginPending)),
            _ => panic!("expected a response"),
        }
        let note = r#"{"event":"ready","persona":"Me"}"#;
        assert!(matches!(
            serde_json::from_str::<ServerMessage>(note).unwrap(),
            ServerMessage::Notification(Notification::Ready { .. })
        ));
    }
}
