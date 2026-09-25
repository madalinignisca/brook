//! Brook shared client core.
//!
//! All non-UI client logic (networking, protocol, state) lives here once and is
//! used by every native client — directly on Linux (GTK4/Qt are also Rust-bound),
//! and via FFI bindings on the other platforms. See `docs/CLIENT_PHILOSOPHY.md`.
//!
//! Phase 1 surface: configure a server, log in over TLS, list/create channels and
//! DMs, send and page message history, and observe realtime [`ServerEvent`]s over
//! a WebSocket — plus the observable [`AuthState`] the UI watches.

mod call;
#[cfg(test)]
mod call_tests;
mod call_types;
mod chat;
mod client;
mod config;
mod error;
#[cfg(test)]
mod log_secrecy_tests;
mod session;
mod session_store;
mod state;
#[cfg(test)]
mod test_support;
mod ws;

pub use call::CallHandle;
pub use call_types::{
    default_labels, CallState, CallStatus, EndReason, EngineError, IceCandidate, IceServer,
    MediaEngine, MediaKind, MediaSource, Participant, PcKind, PublishOffer, Publishing, SubStream,
    TrackLabel,
};
pub use chat::{Channel, ChannelMember, Message, ReactionSummary, ReplyExcerpt};
pub use client::BrookClient;
pub use config::CoreConfig;
pub use error::{Error, Result};
pub use session::{Session, User};
pub use state::AuthState;
pub use ws::ServerEvent;
