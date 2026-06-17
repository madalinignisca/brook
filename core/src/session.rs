//! Session and user types.

use std::fmt;

use serde::Deserialize;

/// A Brook user as returned by the API (`UserOut`).
///
/// Full-field equality is deliberate: the auth state is published on a watch
/// channel, which only notifies receivers when the value changes — so comparing
/// every field ensures metadata updates (e.g. display name) reach the UI.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct User {
    /// Stable user id.
    pub id: String,
    /// Unique handle.
    pub handle: String,
    /// Display name.
    pub display_name: String,
    /// Global role (`admin` or `member`).
    pub global_role: String,
}

/// An authenticated session: tokens plus the resolved user.
#[derive(Clone)]
pub struct Session {
    /// Short-lived access token (Bearer).
    pub access_token: String,
    /// Long-lived, rotatable refresh token.
    pub refresh_token: String,
    /// The authenticated user.
    pub user: User,
}

// Custom Debug so tokens never leak into logs.
impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("user", &self.user)
            .finish()
    }
}
