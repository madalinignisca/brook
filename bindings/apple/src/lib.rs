//! UniFFI bindings of `brook-core` for the Apple clients.
//!
//! `brook-core` stays idiomatic Rust (it is used directly by the GNOME client); every
//! FFI concern — the owned Tokio runtime, the listener callback, FFI-safe types — lives
//! here. See `docs/superpowers/specs/2026-09-24-apple-ffi-bridge-design.md`.

mod call;
mod client;
mod keyslot;
mod listener;
mod offline;
mod runtime;
mod types;

pub use call::{
    CallStateListener, FfiCallHandle, FfiCallState, FfiCallStatus, FfiEndReason, FfiEngineError,
    FfiIceCandidate, FfiIceServer, FfiMediaEngine, FfiMediaKind, FfiMediaSource, FfiParticipant,
    FfiPcKind, FfiServerEvent, FfiSubStream, ServerEventListener,
};
pub use client::FfiBrookClient;
pub use listener::{AuthStateListener, Subscription};
pub use offline::{
    CacheEventListener, CacheStateListener, FfiCacheEvent, FfiCacheState, FfiCachedChannel,
    FfiCachedMessages, FfiDeleted, FfiLocalUser, FfiMember, FfiMessage, FfiOutgoingFile,
    FfiPendingFile, FfiPendingMessage, FfiPendingState, FfiQueuedFile, FfiSendReceipt,
    FfiTransferEvent, FfiTransferState, TransferListener,
};
pub use types::{FfiAuthState, FfiChannel, FfiSession, FfiUser, LoginError, LoginResult};

uniffi::setup_scaffolding!();
