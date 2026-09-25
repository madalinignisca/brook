//! FFI-safe mirrors of `brook-core`'s public types.

use std::sync::Arc;

use brook_core::{AuthState, Error, Session, User};

/// A Brook user (`UserOut`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiUser {
    pub id: String,
    pub handle: String,
    pub display_name: String,
    pub global_role: String,
}

impl From<User> for FfiUser {
    fn from(u: User) -> Self {
        Self {
            id: u.id,
            handle: u.handle,
            display_name: u.display_name,
            global_role: u.global_role,
        }
    }
}

/// Tokens plus the resolved user. Crosses the FFI because the Apple clients keep the
/// tokens in the Keychain; the Swift side redacts them from every description/reflection.
#[derive(Clone, uniffi::Record)]
pub struct FfiSession {
    pub access_token: String,
    pub refresh_token: String,
    pub user: FfiUser,
}

// Same rule as core's `Session`: tokens never reach a log through `Debug`.
impl std::fmt::Debug for FfiSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiSession")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("user", &self.user)
            .finish()
    }
}

impl From<Session> for FfiSession {
    fn from(s: Session) -> Self {
        Self {
            access_token: s.access_token,
            refresh_token: s.refresh_token,
            user: s.user.into(),
        }
    }
}

/// Outcome of a login. An enum from day one so Phase 0b's `TotpRequired` case is
/// additive: Swift switches exhaustively, so every client is told at compile time.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum LoginResult {
    LoggedIn {
        session: FfiSession,
    },
    /// The account has TOTP on: finish with `complete_totp` / `complete_recovery`.
    TotpRequired {
        challenge: Arc<FfiTotpChallenge>,
    },
}

/// The second step of a TOTP sign-in, as an object: the server's pending token never crosses
/// into Swift.
#[derive(uniffi::Object)]
pub struct FfiTotpChallenge {
    pub(crate) inner: brook_core::TotpChallenge,
}

impl std::fmt::Debug for FfiTotpChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(f) // redacted by core
    }
}

#[uniffi::export]
impl FfiTotpChallenge {
    /// Seconds until the server stops accepting it (then sign in with the password again).
    pub fn seconds_left(&self) -> u64 {
        self.inner.seconds_left()
    }
}

/// A started TOTP enrolment, as an object: the URI carries the secret, so it is read on
/// demand (for the QR code and the manual key) rather than living in a printable struct.
#[derive(uniffi::Object)]
pub struct FfiTotpEnrollment {
    pub(crate) inner: brook_core::TotpEnrollment,
}

#[uniffi::export]
impl FfiTotpEnrollment {
    pub fn otpauth_uri(&self) -> String {
        self.inner.otpauth_uri().to_string()
    }
    pub fn expires_in(&self) -> u64 {
        self.inner.expires_in()
    }
}

/// The second factor confirming TOTP off or new recovery codes.
#[derive(Clone, uniffi::Enum)]
pub enum FfiSecondFactor {
    Code { code: String },
    Recovery { code: String },
}

impl From<FfiSecondFactor> for brook_core::SecondFactor {
    fn from(f: FfiSecondFactor) -> Self {
        match f {
            FfiSecondFactor::Code { code } => Self::Code(code),
            FfiSecondFactor::Recovery { code } => Self::Recovery(code),
        }
    }
}

/// The signed-in user with their second-factor state.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiMe {
    pub user: FfiUser,
    pub totp_enabled: bool,
    pub recovery_codes_left: Option<u32>,
}

impl From<brook_core::Me> for FfiMe {
    fn from(m: brook_core::Me) -> Self {
        Self {
            user: m.user.into(),
            totp_enabled: m.totp_enabled,
            recovery_codes_left: m.recovery_codes_left,
        }
    }
}

/// Authentication state as observed by the UI.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiAuthState {
    LoggedOut,
    Authenticating,
    LoggedIn { user: FfiUser },
    Failed { message: String },
}

impl From<AuthState> for FfiAuthState {
    fn from(s: AuthState) -> Self {
        match s {
            AuthState::LoggedOut => Self::LoggedOut,
            AuthState::Authenticating => Self::Authenticating,
            AuthState::LoggedIn(user) => Self::LoggedIn { user: user.into() },
            AuthState::Failed(message) => Self::Failed { message },
        }
    }
}

/// Errors surfaced to Swift. `Api.code` is the server's stable machine code
/// (`docs/PROTOCOL.md` §5), passed through verbatim — UIs key off it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum LoginError {
    #[error("network error: {message}")]
    Network { message: String },
    #[error("invalid server URL: {message}")]
    InvalidServerUrl { message: String },
    #[error("server URL must use https (http is allowed only for localhost)")]
    InsecureServerUrl,
    #[error("server error [{code}]: {message}")]
    Api { code: String, message: String },
    #[error("unexpected response from server")]
    UnexpectedResponse,
    /// The call needs a signed-in session and there is none.
    #[error("not signed in")]
    NotAuthenticated,
    /// A TOTP challenge that is no longer the current sign-in attempt: change nothing.
    #[error("that sign-in attempt is no longer current")]
    ChallengeSuperseded,
    /// The realtime connection is down (or dropped before the server answered).
    #[error("not connected to the server")]
    Disconnected,
    /// The server did not answer a realtime command in time.
    #[error("the server did not answer in time")]
    Timeout,
    /// The call has ended; its handle can no longer be used.
    #[error("the call has ended")]
    CallEnded,
    /// Too many realtime commands are waiting to be sent.
    #[error("too many pending commands")]
    Busy,
    /// A realtime message exceeded the server's size limit.
    #[error("message too large")]
    TooLarge,
}

impl From<Error> for LoginError {
    fn from(e: Error) -> Self {
        match e {
            Error::Http(err) => Self::Network {
                message: err.to_string(),
            },
            Error::Url(err) => Self::InvalidServerUrl {
                message: err.to_string(),
            },
            Error::MissingHost => Self::InvalidServerUrl {
                message: e.to_string(),
            },
            Error::InsecureServerUrl => Self::InsecureServerUrl,
            Error::Api { code, message } => Self::Api { code, message },
            Error::UnexpectedResponse => Self::UnexpectedResponse,
            Error::NotAuthenticated => Self::NotAuthenticated,
            Error::ChallengeSuperseded => Self::ChallengeSuperseded,
            Error::Disconnected => Self::Disconnected,
            Error::Timeout => Self::Timeout,
            Error::CallEnded => Self::CallEnded,
            Error::Busy => Self::Busy,
            Error::TooLarge => Self::TooLarge,
            // The realtime socket is transport, like HTTP: surface it as a network error.
            Error::WebSocket(err) => Self::Network {
                message: err.to_string(),
            },
        }
    }
}

/// A user in the admin user list.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiUserSummary {
    pub id: String,
    pub handle: String,
    pub display_name: String,
    /// `"member"` or `"admin"`.
    pub global_role: String,
}

impl From<brook_core::UserSummary> for FfiUserSummary {
    fn from(u: brook_core::UserSummary) -> Self {
        Self {
            id: u.id,
            handle: u.handle,
            display_name: u.display_name,
            global_role: u.global_role,
        }
    }
}

/// A channel or DM the user belongs to (the fields the Apple UI needs).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiChannel {
    pub id: String,
    /// `"channel"` or `"dm"`.
    pub kind: String,
    pub name: Option<String>,
    pub archived: bool,
}

impl From<brook_core::Channel> for FfiChannel {
    fn from(c: brook_core::Channel) -> Self {
        Self {
            id: c.id,
            kind: c.kind,
            name: c.name,
            archived: c.archived,
        }
    }
}

/// What a restore at launch found (core's `RestoreOutcome`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiRestoreOutcome {
    LoggedIn { user: FfiUser },
    NotSignedIn,
    Unavailable,
    Offline,
    Superseded,
}

impl From<brook_core::RestoreOutcome> for FfiRestoreOutcome {
    fn from(o: brook_core::RestoreOutcome) -> Self {
        match o {
            brook_core::RestoreOutcome::LoggedIn(user) => Self::LoggedIn { user: user.into() },
            brook_core::RestoreOutcome::NotSignedIn => Self::NotSignedIn,
            brook_core::RestoreOutcome::Unavailable => Self::Unavailable,
            brook_core::RestoreOutcome::Offline => Self::Offline,
            brook_core::RestoreOutcome::Superseded => Self::Superseded,
        }
    }
}
