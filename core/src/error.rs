//! Error and result types for the core.

use thiserror::Error;

/// Errors surfaced by the core client.
#[derive(Debug, Error)]
pub enum Error {
    /// Transport/HTTP-level failure (connection, TLS, timeout, ...).
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),

    /// The configured server URL was invalid.
    #[error("invalid server URL: {0}")]
    Url(#[from] url::ParseError),

    /// The server URL has no host.
    #[error("server URL is missing a host")]
    MissingHost,

    /// The server URL is not HTTPS (and not a permitted loopback http URL).
    #[error("server URL must use https (http is allowed only for localhost)")]
    InsecureServerUrl,

    /// A structured error returned by the API (`{code, message}`).
    #[error("server error [{code}]: {message}")]
    Api {
        /// Machine-readable error code (see `docs/PROTOCOL.md`).
        code: String,
        /// Human-readable message.
        message: String,
    },

    /// The server responded in a shape the client could not understand.
    #[error("unexpected response from server")]
    UnexpectedResponse,

    /// An authenticated call was made before logging in.
    #[error("not authenticated")]
    NotAuthenticated,

    /// A TOTP challenge that is no longer the current sign-in attempt (Back, a newer login, a
    /// sign-out, or it was already completed). Nothing was changed; the UI stays as it is.
    #[error("that sign-in attempt is no longer current")]
    ChallengeSuperseded,

    /// A WebSocket transport error (boxed — tungstenite's error is large).
    #[error("websocket error: {0}")]
    WebSocket(Box<tokio_tungstenite::tungstenite::Error>),

    /// The realtime socket is not connected (or dropped before the server answered).
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

    /// A realtime command exceeded the server's frame size limit.
    #[error("message too large")]
    TooLarge,
}

impl From<crate::ws::CommandError> for Error {
    fn from(err: crate::ws::CommandError) -> Self {
        use crate::ws::CommandError as C;
        match err {
            C::NotSent | C::Unknown => Error::Disconnected,
            C::Timeout => Error::Timeout,
            C::Rejected { code, message } => Error::Api { code, message },
            C::UnexpectedReply => Error::UnexpectedResponse,
            C::TooLarge => Error::TooLarge,
            C::Busy => Error::Busy,
        }
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for Error {
    fn from(err: tokio_tungstenite::tungstenite::Error) -> Self {
        Error::WebSocket(Box::new(err))
    }
}

/// Convenience result type for the core.
pub type Result<T> = std::result::Result<T, Error>;
