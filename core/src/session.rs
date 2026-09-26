//! Session and user types.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A Brook user as returned by the API (`UserOut`).
///
/// Full-field equality is deliberate: a re-login as the same user with a changed profile is a
/// different auth state. A profile change within a session isn't re-published there (see
/// `AuthState::LoggedIn`); `session.user` and the calls that change it carry it.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct User {
    /// Stable user id.
    pub id: String,
    /// Unique handle.
    pub handle: String,
    /// Display name.
    pub display_name: String,
    /// Global role (`admin` or `member`).
    pub global_role: String,
    /// The line under the name the user sets (`PATCH /auth/me`); none when unset or from a
    /// server before it. Not `status`: that's the account state (active or disabled).
    /// The server stores an unset line as `""`; it's read as none, so callers have one
    /// "unset", not two.
    #[serde(
        default,
        deserialize_with = "empty_as_none",
        skip_serializing_if = "Option::is_none"
    )]
    pub status_text: Option<String>,
}

fn empty_as_none<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let s: Option<String> = serde::Deserialize::deserialize(d)?;
    Ok(s.filter(|s| !s.is_empty()))
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
