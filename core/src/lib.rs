//! Brook shared client core.
//!
//! All non-UI client logic (networking, protocol, state) lives here once and is
//! used by every native client — directly on Linux (GTK4/Qt are also Rust-bound),
//! and via FFI bindings on the other platforms. See `docs/CLIENT_PHILOSOPHY.md`.
//!
//! Phase 1 surface: configure a server, log in over TLS, list/create channels and
//! DMs, send and page message history, and observe realtime [`ServerEvent`]s over
//! a WebSocket — plus the observable [`AuthState`] the UI watches.

mod account;
#[cfg(test)]
mod account_tests;
#[allow(dead_code)] // wired up by the sync loop (C2) and the read API (C3)
mod apply;
#[cfg(test)]
mod apply_tests;
#[allow(dead_code)] // wired into the client once stores turn on (C5)
mod cache;
#[allow(dead_code)]
mod cache_http;
#[cfg(test)]
mod cache_tests;
mod call;
#[cfg(test)]
mod call_tests;
mod call_types;
mod chat;
mod client;
mod config;
#[allow(dead_code)] // read by cached_messages (C3)
mod coverage;
#[cfg(test)]
mod coverage_tests;
mod error;
mod keyslot;
#[cfg(test)]
mod log_secrecy_tests;
mod persist;
#[cfg(test)]
mod restore_tests;
mod session;
mod session_store;
#[cfg(test)]
mod signout_tests;
mod state;
#[allow(dead_code)] // wired up by C2-C5; C1 lands the layer and its tests
mod store;
#[cfg(test)]
mod store_tests;
#[allow(dead_code)] // wired into the client with the read API (C3)
mod sync;
#[cfg(test)]
mod sync_tests;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support;
#[cfg(test)]
mod totp_tests;
mod ws;

pub use account::{Me, SecondFactor, TotpEnrollment, UserSummary};
pub use call::CallHandle;
pub use call_types::{
    default_labels, CallState, CallStatus, EndReason, EngineError, IceCandidate, IceServer,
    MediaEngine, MediaKind, MediaSource, Participant, PcKind, PublishOffer, Publishing, SubStream,
    TrackLabel,
};
pub use chat::{Channel, ChannelMember, Message, ReactionSummary, ReplyExcerpt};
pub use client::{BrookClient, LoginOutcome, RestoreOutcome, TotpChallenge};
pub use config::CoreConfig;
pub use error::{Error, Result};
pub use keyslot::{InMemoryKeySlot, KeySlot, KeySlotError, KeyStore};
pub use session::{Session, User};
pub use state::AuthState;
pub use ws::ServerEvent;
