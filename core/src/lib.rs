//! Brook shared client core.
//!
//! All non-UI client logic (networking, protocol, state) lives here once and is
//! used by every native client — directly on Linux (GTK4/Qt are also Rust-bound),
//! and via FFI bindings on the other platforms. See `docs/CLIENT_PHILOSOPHY.md`.
//!
//! Phase 1 surface: configure a server, log in over TLS, list/create channels and
//! DMs, send and page message history, and observe realtime [`ServerEvent`]s over
//! a WebSocket — plus the observable [`AuthState`] the UI watches.

mod chat;
mod client;
mod config;
mod error;
mod session;
mod state;
mod ws;

pub use chat::{Channel, ChannelMember, Message, ReactionSummary, ReplyExcerpt};
pub use client::BrookClient;
pub use config::CoreConfig;
pub use error::{Error, Result};
pub use session::{Session, User};
pub use state::AuthState;
pub use ws::ServerEvent;
