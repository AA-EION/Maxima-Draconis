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

/// Where a launched game's entitlement is reported to come from.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EntitlementSource {
    Ea,
    Steam,
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
        /// Wine prefix (unix hosts) to use for this request instead of the
        /// game's own. Omitted = the server picks per game.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wine_prefix: Option<String>,
        /// Extra Wine DLL overrides for this launch, each `dll[,dll]=mode`.
        #[serde(default)]
        wine_dll_overrides: Vec<String>,
        /// Steam App ID to expose to the game (`SteamAppId` / `SteamGameId`).
        #[serde(default)]
        steam_app_id: Option<String>,
        /// Overrides the entitlement source otherwise derived from
        /// `steam_app_id` (Steam when set, EA when not).
        #[serde(default)]
        entitlement_source: Option<EntitlementSource>,
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
        /// Wine prefix (unix hosts) to use for this request instead of the
        /// game's own. Omitted = the server picks per game.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wine_prefix: Option<String>,
        /// Glob patterns of files to leave out of the download, on top of
        /// the game's exclusion file. Remembered in the install record.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude: Vec<String>,
    },
    LocateGame {
        path: String,
        /// The game the folder belongs to; when omitted the server looks the
        /// folder up in its install records.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slug: Option<String>,
        /// Wine prefix (unix hosts) to use for this request instead of the
        /// game's own. Omitted = the server picks per game.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wine_prefix: Option<String>,
    },
    CloudSync {
        slug: String,
        #[serde(default)]
        write: bool,
        /// Wine prefix (unix hosts) to use for this request instead of the
        /// game's own. Omitted = the server picks per game.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wine_prefix: Option<String>,
    },
    /// Size-verify a game's files against the build manifest; `repair`
    /// re-downloads the broken ones.
    Verify {
        slug: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        repair: bool,
        /// Wine prefix (unix hosts) to use for this request instead of the
        /// game's own. Omitted = the server picks per game.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wine_prefix: Option<String>,
        /// Extra glob patterns of files verify must not count as missing.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude: Vec<String>,
    },
    /// Download a single named file from a game's build manifest.
    DownloadFile {
        slug: String,
        #[serde(default)]
        build_id: Option<String>,
        file: String,
        /// Wine prefix (unix hosts) to use for this request instead of the
        /// game's own. Omitted = the server picks per game.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wine_prefix: Option<String>,
    },
    /// Read-only bottle / prefix / game-dir readout (creates nothing).
    BottleInfo {
        slug: String,
        /// Wine prefix (unix hosts) to use for this request instead of the
        /// game's own. Omitted = the server picks per game.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wine_prefix: Option<String>,
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
                slug: "example-game".into(),
                args: vec!["--flag".into()],
                exe_override: None,
                cloud_saves: true,
                wine_prefix: None,
                wine_dll_overrides: vec!["wsock32=n,b".into()],
                steam_app_id: Some("12345".into()),
                entitlement_source: Some(EntitlementSource::Steam),
            },
        };
        let s = serde_json::to_string(&env).unwrap();
        // id + flattened tagged command, matching the historical wire.
        assert!(s.contains("\"id\":7"));
        assert!(s.contains("\"cmd\":\"launch\""));
        assert!(s.contains("\"slug\":\"example-game\""));
        assert!(s.contains("\"wine_dll_overrides\":[\"wsock32=n,b\"]"));
        assert!(s.contains("\"steam_app_id\":\"12345\""));
        assert!(s.contains("\"entitlement_source\":\"steam\""));
    }

    #[test]
    fn launch_optional_fields_default() {
        let req: RequestEnvelope =
            serde_json::from_str(r#"{"id":1,"cmd":"launch","slug":"example-game"}"#).unwrap();
        match req.request {
            Request::Launch {
                args,
                exe_override,
                cloud_saves,
                wine_dll_overrides,
                steam_app_id,
                entitlement_source,
                ..
            } => {
                assert!(args.is_empty());
                assert!(exe_override.is_none());
                assert!(cloud_saves);
                assert!(wine_dll_overrides.is_empty());
                assert!(steam_app_id.is_none());
                assert!(entitlement_source.is_none());
            }
            _ => panic!("expected launch"),
        }
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
    fn new_request_fields_are_omitted_when_unset_and_optional_on_read() {
        let env = RequestEnvelope {
            id: 1,
            request: Request::Install {
                slug: "g".into(),
                path: None,
                build_id: None,
                replace_files: vec![],
                only_listed_files: false,
                wine_prefix: None,
                exclude: vec![],
            },
        };
        let s = serde_json::to_string(&env).unwrap();
        assert!(!s.contains("wine_prefix"));
        assert!(!s.contains("exclude"));

        // A request from a client that predates the fields still parses.
        let old = r#"{"id":2,"cmd":"install","slug":"g","path":"/p"}"#;
        let parsed: RequestEnvelope = serde_json::from_str(old).unwrap();
        match parsed.request {
            Request::Install { wine_prefix, exclude, .. } => {
                assert_eq!(wine_prefix, None);
                assert!(exclude.is_empty());
            }
            other => panic!("unexpected {other:?}"),
        }

        let new = r#"{"id":3,"cmd":"verify","slug":"g","wine_prefix":"/w","exclude":["*.bik"]}"#;
        let parsed: RequestEnvelope = serde_json::from_str(new).unwrap();
        match parsed.request {
            Request::Verify { wine_prefix, exclude, .. } => {
                assert_eq!(wine_prefix.as_deref(), Some("/w"));
                assert_eq!(exclude, vec!["*.bik".to_string()]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn locate_game_slug_is_optional() {
        let old = r#"{"id":4,"cmd":"locate-game","path":"/g"}"#;
        let parsed: RequestEnvelope = serde_json::from_str(old).unwrap();
        assert!(matches!(
            parsed.request,
            Request::LocateGame { slug: None, wine_prefix: None, .. }
        ));
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
