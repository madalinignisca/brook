//! Observable authentication state.

use crate::session::User;

/// The client's authentication state, published on a watch channel so the UI
/// can react without owning the networking logic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AuthState {
    /// No active session.
    #[default]
    LoggedOut,
    /// A login attempt is in flight.
    Authenticating,
    /// Logged in as the given user.
    LoggedIn(User),
    /// The last attempt failed, with a human-readable reason.
    Failed(String),
}
