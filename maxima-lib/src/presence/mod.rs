//! Friends / presence, behind a pluggable backend.
//!
//! EA has more than one way to talk presence and we cannot verify which one is
//! live without an EA account, so the transport is an implementation detail of
//! a [`PresenceBackend`]:
//!
//! * [`PresenceBackendKind::Legacy`] - the original RTM client (`rtm.tnt-ea.com`).
//!   This is the default and behaves exactly as it always did.
//! * [`PresenceBackendKind::Antelope`] - the same RTM transport speaking the
//!   payload dialect of upstream PR 70's "Antelope" rewrite.
//! * [`PresenceBackendKind::Grpc`] - EA's "social" gRPC presence service
//!   (upstream PR 67), behind the off-by-default `presence-grpc` cargo feature.
//!
//! Selection is `MAXIMA_PRESENCE_BACKEND=legacy|antelope|grpc|auto`, default
//! `legacy`. `auto` tries gRPC (when compiled in) and falls back to legacy when
//! the gRPC login fails. Everything here is best-effort: a backend failing must
//! never take a game launch or an LSX session down.
//!
//! Whatever the backend, friend presences land in one shared store and are
//! announced on one event stream, which is what the server and the UI consume.

pub mod backend;
pub mod client;
pub mod model;

#[cfg(feature = "presence-grpc")]
pub mod grpc;

pub use backend::{BackendSelection, PresenceBackend, PresenceBackendKind};
pub use client::PresenceClient;
pub use model::{
    BasicPresence, LockedPresenceStore, PresenceEvent, PresenceSink, PresenceUpdate, RichPresence,
    RichPresenceBuilder,
};

/// Presence errors share [`crate::rtm::RtmError`] so that existing callers
/// (`LSXRequestError::Rtm`, the UI's bridge errors) keep working unchanged.
pub type PresenceError = crate::rtm::RtmError;
