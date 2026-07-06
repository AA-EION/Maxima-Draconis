//! `maxima-proto` — the RPC glue between the Maxima server and its frontends
//! (upstream PR #23's `maxima_proto`). It defines the wire DTOs, the typed
//! request/response/notification envelopes, and a real async [`MaximaClient`].
//!
//! It has NO dependency on `maxima-lib`, so a frontend can be a true thin
//! client — holding a [`MaximaClient`] and rendering server state — without
//! linking any of the server's EA / auth / LSX / download logic.
//!
//! The wire is newline-delimited JSON whose shapes exactly match what the
//! server historically emitted, so the SwiftUI app (which speaks that JSON
//! directly) needs no changes when the Rust side moves to these typed
//! messages.

pub mod client;
pub mod message;
pub mod types;

pub use client::{server_port, ClientError, MaximaClient, DEFAULT_PORT};
pub use message::{
    Notification, Request, RequestEnvelope, ResponseEnvelope, ServerMessage,
};
pub use types::{
    ExtraOfferDto, FriendDto, GameDetailsDto, GameDto, GameImagesDto, PresenceDto, StatusDto,
    UserDto,
};
