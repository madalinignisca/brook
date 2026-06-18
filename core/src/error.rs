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

    /// A WebSocket transport error (boxed — tungstenite's error is large).
    #[error("websocket error: {0}")]
    WebSocket(Box<tokio_tungstenite::tungstenite::Error>),
}

impl From<tokio_tungstenite::tungstenite::Error> for Error {
    fn from(err: tokio_tungstenite::tungstenite::Error) -> Self {
        Error::WebSocket(Box::new(err))
    }
}

/// Convenience result type for the core.
pub type Result<T> = std::result::Result<T, Error>;
