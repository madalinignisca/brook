//! Staying signed in (plan docs/superpowers/specs/2026-09-25-keyslot-session-plan.md P2).
//!
//! The stored session mirrors the live one **write-through**: every write or clear runs inside
//! the session store's own write section, next to the change it mirrors, so the store's order
//! is the persistence order. A sign-out that can't delete the stored copy writes a **fence**
//! (a small non-secret file); a fenced origin is never restored.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{KeySlot, Session, User};

/// What the slot holds. The access token is never stored.
#[derive(Serialize, Deserialize)]
pub(crate) struct Stored {
    pub(crate) user: User,
    pub(crate) refresh_token: String,
}

impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stored")
            .field("user", &self.user)
            .finish_non_exhaustive() // the refresh token is never shown
    }
}

/// Which client owns each slot in this process: the newest one to enable persistence. Every
/// client orders its own writes, but clients share `session:<origin>` and nothing orders one
/// client's writes against another's: an older client's late rotation, rejection, restore or
/// sign-out could overwrite, delete or resurrect the newer one's session. So only the owner
/// touches the slot, and the map's lock is held for the slot operation itself. (The app makes
/// a new client for every attempt, so the owner is always the current attempt's client.)
static OWNERS: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(Mutex::default);

/// One refresh at a time per slot, process-wide (#120): every client refreshing a token of
/// the stored login goes through its slot's lock, so an old client's refresh in flight
/// finishes before a new owner's restore reads the stored token.
static SLOT_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

/// Which login each slot holds, as a generation: a fresh login (password, TOTP) written to
/// the slot starts a new one; a restore continues it. A client's token is the stored login's
/// only while its generation is the slot's current one (#120).
static FAMILIES: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(Mutex::default);

pub(crate) struct Persistence {
    slot: Arc<dyn KeySlot>,
    name: String,
    fence_dir: PathBuf,
    fence: PathBuf,
}

/// A stable, non-cryptographic name for an origin (FNV-1a): no server name in file paths.
fn stable_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

impl Persistence {
    pub(crate) fn new(slot: Arc<dyn KeySlot>, origin: &str, data_dir: PathBuf) -> Self {
        let fence_dir = data_dir.join("signed-out");
        let fence = fence_dir.join(stable_hash(origin));
        Self {
            slot,
            name: format!("session:{origin}"),
            fence_dir,
            fence,
        }
    }

    fn owners() -> std::sync::MutexGuard<'static, HashMap<String, u64>> {
        OWNERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Client `owner` (a store id) now owns this slot; every older client's operations on it
    /// become no-ops.
    pub(crate) fn claim(&self, owner: u64) {
        Self::owners().insert(self.name.clone(), owner);
    }

    /// Whether client `owner` owns this slot (a map read; no keychain call under the lock).
    pub(crate) fn owns(&self, owner: u64) -> bool {
        Self::owners().get(&self.name) == Some(&owner)
    }

    fn families() -> std::sync::MutexGuard<'static, HashMap<String, u64>> {
        FAMILIES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A fresh login is now stored: a new generation.
    pub(crate) fn new_family(&self) -> u64 {
        let mut f = Self::families();
        let next = f.get(&self.name).copied().unwrap_or(0) + 1;
        f.insert(self.name.clone(), next);
        next
    }

    /// The generation of the login stored now.
    pub(crate) fn family(&self) -> u64 {
        Self::families().get(&self.name).copied().unwrap_or(0)
    }

    /// This slot's refresh lock (see `SLOT_LOCKS`).
    pub(crate) fn slot_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        SLOT_LOCKS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(self.name.clone())
            .or_default()
            .clone()
    }

    /// Run `op` on the slot only if client `owner` owns it. None: a newer client owns it and
    /// nothing was done.
    pub(crate) fn guarded<T>(&self, owner: u64, op: impl FnOnce(&Self) -> T) -> Option<T> {
        let owners = Self::owners();
        if owners.get(&self.name) != Some(&owner) {
            return None;
        }
        Some(op(self))
    }

    pub(crate) fn load(&self) -> Result<Option<Stored>, crate::KeySlotError> {
        match self.slot.load(self.name.clone())? {
            None => Ok(None),
            Some(bytes) => {
                let bytes = Zeroizing::new(bytes);
                // Unparseable bytes are a fault, not "absent" (never a reason to delete).
                serde_json::from_slice(&bytes)
                    .map(Some)
                    .map_err(|_| crate::KeySlotError::Fatal(-2))
            }
        }
    }

    /// Mirror an installed or committed session. On success the fence (if any) is lifted; on
    /// failure the stored copy is stale, so it is fenced. Whether it landed.
    pub(crate) fn write(&self, session: &Session) -> bool {
        let stored = Stored {
            user: session.user.clone(),
            refresh_token: session.refresh_token.clone(),
        };
        let bytes = Zeroizing::new(serde_json::to_vec(&stored).unwrap_or_default());
        match self.slot.replace(self.name.clone(), bytes.to_vec()) {
            Ok(()) => {
                self.lift_fence();
                true
            }
            Err(err) => {
                tracing::warn!(%err, "storing the session failed; fencing the stale copy");
                let _ = self.write_fence();
                false
            }
        }
    }

    /// Make the stored session unusable: delete it, or fence it. False only when both failed.
    pub(crate) fn clear(&self) -> bool {
        match self.slot.delete(self.name.clone()) {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(%err, "deleting the stored session failed; fencing it");
                self.write_fence().is_ok()
            }
        }
    }

    /// Delete the stored session only if it still holds `refresh_token` (a stale restore must
    /// not delete a newer sign-in's session).
    pub(crate) fn clear_if_holds(&self, refresh_token: &str) {
        if let Ok(Some(stored)) = self.load() {
            if stored.refresh_token == refresh_token {
                let _ = self.clear();
            }
        }
    }

    /// The client is gone but the server already rotated `rotated_from` into `fresh`: the
    /// stored token is dead, so store the fresh one, if the stored copy is still the one that
    /// was rotated. True: stored (the caller must not revoke it).
    pub(crate) fn follow_rotation(&self, rotated_from: &str, fresh: &str) -> bool {
        let Ok(Some(stored)) = self.load() else {
            return false;
        };
        if stored.refresh_token != rotated_from {
            return false;
        }
        let next = Stored {
            user: stored.user,
            refresh_token: fresh.to_owned(),
        };
        let bytes = Zeroizing::new(serde_json::to_vec(&next).unwrap_or_default());
        self.slot.replace(self.name.clone(), bytes.to_vec()).is_ok()
    }

    /// Re-store `session`'s user beside the same refresh token: a profile change, not a
    /// credential one. Unlike [`Persistence::write`], it never fences (a failure leaves a
    /// restorable copy with an older name) and never lifts a fence (a fenced slot may hold a
    /// token no family marks, #140). It writes only when the stored record still holds this
    /// session's token: a failed or empty read, or another token, skips it. Runs under the
    /// owner guard, which holds `OWNERS` across the read and the write.
    pub(crate) fn rewrite_user(&self, session: &Session) -> bool {
        if self.fenced() {
            return false;
        }
        let Ok(Some(stored)) = self.load() else {
            return false;
        };
        if stored.refresh_token != session.refresh_token {
            return false;
        }
        let next = Stored {
            user: session.user.clone(),
            refresh_token: stored.refresh_token,
        };
        let bytes = Zeroizing::new(serde_json::to_vec(&next).unwrap_or_default());
        match self.slot.replace(self.name.clone(), bytes.to_vec()) {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(%err, "re-storing the profile failed; the stored one stays");
                false
            }
        }
    }

    /// Whether this origin is fenced. Fails closed: an unreadable fence directory is a fence.
    pub(crate) fn fenced(&self) -> bool {
        match fs::metadata(&self.fence) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match fs::read_dir(&self.fence_dir) {
                    Ok(_) => false,
                    Err(e) => e.kind() != std::io::ErrorKind::NotFound,
                }
            }
            Err(_) => true,
        }
    }

    /// Atomically: a temp file, fsync, rename, fsync of the directory and of its parent (a new
    /// directory's entry must be durable too, or the fence in it may not survive a power loss).
    fn write_fence(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.fence_dir)?;
        if let Some(parent) = self.fence_dir.parent() {
            fs::File::open(parent)?.sync_all()?; // every time: an earlier attempt may have failed here
        }
        let tmp = self.fence_dir.join(format!(".{}.tmp", std::process::id()));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(b"signed out\n")?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.fence)?;
        fs::File::open(&self.fence_dir)?.sync_all()
    }

    fn lift_fence(&self) {
        let _ = fs::remove_file(&self.fence);
    }
}
