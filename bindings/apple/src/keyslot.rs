//! The platform's secure store, implemented in Swift (the data-protection Keychain), used by
//! core for staying signed in (plan 2026-09-25-keyslot-session-plan.md P3).

use std::sync::Arc;

use brook_core::{KeySlot, KeySlotError};

/// Why a slot operation failed (see core's `KeySlotError`). `Fatal` carries a numeric status only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum FfiKeySlotError {
    #[error("the slot already exists")]
    Exists,
    #[error("key storage is unavailable")]
    Unavailable,
    #[error("key storage failed (status {status})")]
    Fatal { status: i32 },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for FfiKeySlotError {
    fn from(_: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Fatal { status: -3 } // a Swift-side fault: never a reason to delete anything
    }
}

/// Named-slot byte storage, implemented in Swift. Synchronous, like core's trait.
#[uniffi::export(with_foreign)]
pub trait FfiKeySlot: Send + Sync {
    fn load(&self, slot: String) -> Result<Option<Vec<u8>>, FfiKeySlotError>;
    fn create(&self, slot: String, bytes: Vec<u8>) -> Result<(), FfiKeySlotError>;
    fn replace(&self, slot: String, bytes: Vec<u8>) -> Result<(), FfiKeySlotError>;
    fn delete(&self, slot: String) -> Result<(), FfiKeySlotError>;
}

pub(crate) struct SlotAdapter(pub(crate) Arc<dyn FfiKeySlot>);

fn map(err: FfiKeySlotError) -> KeySlotError {
    match err {
        FfiKeySlotError::Exists => KeySlotError::Exists,
        FfiKeySlotError::Unavailable => KeySlotError::Unavailable,
        FfiKeySlotError::Fatal { status } => KeySlotError::Fatal(status),
    }
}

impl KeySlot for SlotAdapter {
    fn load(&self, slot: String) -> Result<Option<Vec<u8>>, KeySlotError> {
        self.0.load(slot).map_err(map)
    }
    fn create(&self, slot: String, bytes: Vec<u8>) -> Result<(), KeySlotError> {
        self.0.create(slot, bytes).map_err(map)
    }
    fn replace(&self, slot: String, bytes: Vec<u8>) -> Result<(), KeySlotError> {
        self.0.replace(slot, bytes).map_err(map)
    }
    fn delete(&self, slot: String) -> Result<(), KeySlotError> {
        self.0.delete(slot).map_err(map)
    }
}
