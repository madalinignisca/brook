//! Brook shared client core.
//!
//! All non-UI client logic (networking, protocol, state) lives here once and is
//! used by every native client — directly on Linux (GTK4/Qt are also Rust-bound),
//! and via FFI bindings on the other platforms. See `docs/CLIENT_PHILOSOPHY.md`.
//!
//! Phase 0 surface: configure a server, log in over TLS, and expose an observable
//! [`AuthState`] the UI can watch.

mod client;
mod config;
mod error;
mod session;
mod state;

pub use client::BrookClient;
pub use config::CoreConfig;
pub use error::{Error, Result};
pub use session::{Session, User};
pub use state::AuthState;
