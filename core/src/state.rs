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
    /// Logged in as the given user, as of the sign-in: a later profile change doesn't
    /// re-publish it (clients read `LoggedIn` as a sign-in). The current profile is the
    /// restore outcome's, `update_profile`'s answer, and `current_user_id` / `is_admin`.
    LoggedIn(User),
    /// The last attempt failed, with a human-readable reason.
    Failed(String),
}
