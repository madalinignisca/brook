//! Brook shared client core.
//!
//! All non-UI client logic (networking, protocol, state) lives here once and is
//! used by every native client — directly on Linux (GTK4/Qt are also Rust-bound),
//! and via FFI bindings on the other platforms. See `docs/CLIENT_PHILOSOPHY.md`.
//!
//! Phase 1 surface: configure a server, log in over TLS, list/create channels and
//! DMs, send and page message history, and observe realtime [`ServerEvent`]s over
//! a WebSocket — plus the observable [`AuthState`] the UI watches.

mod account;
#[cfg(test)]
mod account_tests;
mod apply;
#[cfg(test)]
mod apply_tests;
mod cache;
mod cache_http;
#[cfg(test)]
mod cache_tests;
mod call;
#[cfg(test)]
mod call_tests;
mod call_types;
mod chat;
mod client;
mod client_offline;
mod config;
mod coverage;
#[cfg(test)]
mod coverage_tests;
mod error;
mod file_rows;
#[cfg(test)]
mod file_rows_tests;
mod files;
#[cfg(test)]
mod files_tests;
mod keyslot;
mod local;
#[cfg(test)]
mod local_tests;
#[cfg(test)]
mod log_secrecy_tests;
mod offline;
#[cfg(test)]
mod offline_tests;
mod outbox;
#[cfg(test)]
mod outbox_tests;
mod persist;
#[cfg(test)]
mod restore_tests;
mod session;
mod session_store;
#[cfg(test)]
mod signout_tests;
mod snapshot;
#[cfg(test)]
mod snapshot_tests;
mod state;
mod store;
#[cfg(test)]
mod store_tests;
mod sync;
#[cfg(test)]
mod sync_tests;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support;
#[cfg(test)]
mod totp_tests;
mod transfer;
mod ws;

pub use account::{Me, SecondFactor, TotpEnrollment, UserSummary};
pub use cache::{CacheEvent, CacheState};
pub use call::CallHandle;
pub use call_types::{
    default_labels, CallState, CallStatus, EndReason, EngineError, IceCandidate, IceServer,
    MediaEngine, MediaKind, MediaSource, Participant, PcKind, PublishOffer, Publishing, SubStream,
    TrackLabel,
};
pub use chat::{Channel, ChannelMember, Message, ReactionSummary, ReplyExcerpt};
pub use client::{BrookClient, LoginOutcome, RestoreOutcome, TotpChallenge};
pub use client_offline::CachedMessages;
pub use config::CoreConfig;
pub use error::{Error, Result};
pub use files::{FileCacheState, FILE_CACHE_CAP};
pub use keyslot::{InMemoryKeySlot, KeySlot, KeySlotError, KeyStore};
pub use outbox::{
    Deleted, OutgoingFile, PendingFile, PendingMessage, PendingState, QueuedFile, SendReceipt,
    MAX_FILES_PER_MESSAGE, MAX_FILE_BYTES,
};
pub use session::{Session, User};
pub use state::AuthState;
pub use transfer::{
    is_transient, DownloadSink, FileInfo, FileSink, FileSource, SinkError, TransferEvent,
    TransferId, TransferState, UploadSource,
};
pub use ws::ServerEvent;
