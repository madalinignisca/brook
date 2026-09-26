//! Local data per (server, user) (plan C5, spec §3 and §7): which stores exist, opening them,
//! and wiping them.
//!
//! Layout: `<root>/index.db` maps `(origin, user id)` to a random store id, and each store id
//! is a directory holding `cache.db` and `outbox.db` (plus their key checks). Paths carry
//! only store ids.
//!
//! **Wiping is close, then erase.** The caller closes the user's cache and outbox first
//! (their threads are joined), then `wipe` destroys the key slots (crypto-erase) and deletes
//! the directory. A late writer holds a closed handle and can't reach anything (a store made
//! later has a new key and a new check file), which is why no per-store generation counter
//! is needed: close-and-join gives the same guarantee.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::store::{self, Db, Kind, Opened, Rebuilt, StoreError};
use crate::{KeySlot, KeyStore};

/// A user's two stores, as opened.
pub(crate) struct UserStores {
    #[cfg_attr(not(test), allow(dead_code))] // tests follow a store by its id
    pub(crate) store_id: String,
    pub(crate) cache: Opened,
    pub(crate) outbox: Opened,
    /// The store's directory (the outbox keeps its snapshots under it).
    pub(crate) dir: PathBuf,
}

pub(crate) struct LocalData {
    root: PathBuf,
    keys: KeyStore<dyn KeySlot>,
    index: Db,
    /// Unsent messages on this device were lost: reconciliation erased an ownerless outbox
    /// that held some, or a user's outbox couldn't be kept (its key is gone, or its format
    /// changed). Set before the rebuild that loses them, so a rebuild failing part-way
    /// can't drop the report. Said once.
    lost_unsent: Arc<std::sync::atomic::AtomicBool>,
}

impl LocalData {
    /// Open the index under `root`. `None` when the key store is locked or damaged: the app
    /// runs online-only, and nothing is deleted.
    pub(crate) async fn open(
        root: &Path,
        slot: Arc<dyn KeySlot>,
    ) -> Result<Option<Self>, StoreError> {
        let keys = KeyStore::new(slot);
        let (root, k) = (root.to_path_buf(), KeyStore::new(keys.slots()));
        let opened = tokio::task::spawn_blocking({
            let root = root.clone();
            move || store::open(&root, Kind::Index, "", &k)
        })
        .await
        .map_err(|_| StoreError::Io)??;
        let (index, rebuilt) = match opened {
            Opened::Ready { db, rebuilt } => (db, rebuilt),
            _ => return Ok(None),
        };
        let local = Self {
            root,
            keys,
            index,
            lost_unsent: Arc::default(),
        };
        if rebuilt == Some(Rebuilt::KeyMissing) {
            // The index's key is gone: nobody knows which directory is whose any more, so
            // every store is an orphan. Guessing an owner could show one user's data to
            // another, so `reconcile` erases them, and says so if an outbox was among them.
            // A failure here is left to the caller's own reconcile, which retries it: returning
            // the error would drop `local`, and with it a loss already found.
            let _ = local.reconcile().await;
        }
        Ok(Some(local))
    }

    fn dir(&self, store_id: &str) -> PathBuf {
        self.root.join(store_id)
    }

    /// Open (or make) the stores of `(origin, user_id)`.
    pub(crate) async fn open_user(
        &self,
        origin: &str,
        user_id: &str,
    ) -> Result<UserStores, StoreError> {
        // A wipe that couldn't destroy the keys left its row doomed: finish it before this
        // user gets stores again (reusing that id would reuse the old keys).
        if self.doomed_id(origin, user_id).await?.is_some() {
            self.wipe(origin, user_id).await?;
        }
        let store_id = store::store_id(&self.index, origin, user_id).await?;
        let dir = self.dir(&store_id);
        let keys = KeyStore::new(self.keys.slots());
        let id = store_id.clone();
        let lost = self.lost_unsent.clone();
        let (cache, outbox) = tokio::task::spawn_blocking(move || {
            use std::sync::atomic::Ordering;
            let cache = store::open(&dir, Kind::Cache, &id, &keys)?;
            let mut outbox = store::open(&dir, Kind::Outbox, &id, &keys)?;
            if matches!(
                outbox,
                Opened::Ready {
                    rebuilt: Some(Rebuilt::KeyMissing),
                    ..
                }
            ) {
                lost.store(true, Ordering::SeqCst);
            }
            if let Opened::NeedsRebuild { unsent } = outbox {
                // A format change: remade (pre-1.0: no migration). A loss only if something
                // was waiting (or can't be counted), flagged before the rebuild so one that
                // fails still reports it.
                if unsent != Some(0) {
                    lost.store(true, Ordering::SeqCst);
                }
                outbox = store::rebuild(&dir, Kind::Outbox, &id, &keys)?;
            }
            Ok::<_, StoreError>((cache, outbox))
        })
        .await
        .map_err(|_| StoreError::Io)??;
        Ok(UserStores {
            dir: self.dir(&store_id),
            store_id,
            cache,
            outbox,
        })
    }

    async fn doomed_id(&self, origin: &str, user_id: &str) -> Result<Option<String>, StoreError> {
        let (o, u) = (origin.to_string(), user_id.to_string());
        self.index
            .call(move |c| {
                use rusqlite::OptionalExtension;
                c.query_row(
                    "SELECT store_id FROM stores WHERE origin = ?1 AND user_id = ?2 AND doomed = 1",
                    [&o, &u],
                    |r| r.get(0),
                )
                .optional()
            })
            .await
    }

    /// Erase `(origin, user_id)`'s stores. Their handles must be closed first. The row is
    /// marked doomed first; then the key slots go (crypto-erase), then the files; the row goes
    /// only once every key is destroyed. A key store that refused keeps the row doomed and
    /// this reports an error: the next open of that user, or `reconcile`, finishes it (the
    /// old keys are never reused). Works on a locked or damaged store: nothing is read.
    pub(crate) async fn wipe(&self, origin: &str, user_id: &str) -> Result<(), StoreError> {
        let (o, u) = (origin.to_string(), user_id.to_string());
        let found: Option<String> = self
            .index
            .call(move |c| {
                use rusqlite::OptionalExtension;
                let id: Option<String> = c
                    .query_row(
                        "SELECT store_id FROM stores WHERE origin = ?1 AND user_id = ?2",
                        [&o, &u],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(id) = &id {
                    c.execute("UPDATE stores SET doomed = 1 WHERE store_id = ?1", [id])?;
                }
                Ok(id)
            })
            .await?;
        let Some(store_id) = found else {
            return Ok(());
        };
        self.erase_row(&store_id).await
    }

    /// Erase a doomed row's store; the row goes only if every key was destroyed.
    async fn erase_row(&self, store_id: &str) -> Result<(), StoreError> {
        if !self.erase(store_id).await? {
            return Err(StoreError::Io); // a key survived: the row stays doomed
        }
        let id = store_id.to_string();
        self.index
            .call(move |c| c.execute("DELETE FROM stores WHERE store_id = ?1", [&id]))
            .await?;
        Ok(())
    }

    /// The users with stores here other than `(origin, user_id)`: a different user signing
    /// in wipes them (#46 §8), after the app surfaced their unsent messages.
    pub(crate) async fn others(
        &self,
        origin: &str,
        user_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let (o, u) = (origin.to_string(), user_id.to_string());
        self.index
            .call(move |c| {
                c.prepare(
                    "SELECT origin, user_id FROM stores WHERE NOT (origin = ?1 AND user_id = ?2)",
                )?
                .query_map([&o, &u], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect()
            })
            .await
    }

    /// As [`LocalData::others`], each with how many messages its outbox holds unsent, or
    /// `None` when that can't be read (#46 §8: the app names them before the wipe, and "may
    /// have included unsent messages" beats a silent loss).
    pub(crate) async fn others_with_unsent(
        &self,
        origin: &str,
        user_id: &str,
    ) -> Result<Vec<(String, String, Option<u64>)>, StoreError> {
        let (o, u) = (origin.to_string(), user_id.to_string());
        let rows: Vec<(String, String, String)> = self
            .index
            .call(move |c| {
                c.prepare(
                    "SELECT origin, user_id, store_id FROM stores
                     WHERE NOT (origin = ?1 AND user_id = ?2)",
                )?
                .query_map([&o, &u], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect()
            })
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (origin, user, store_id) in rows {
            let unsent = self.unsent_in(&store_id).await;
            out.push((origin, user, unsent));
        }
        Ok(out)
    }

    /// Both stores are attempted whatever the other did. The directory goes only if neither
    /// refused (a store still open refuses: its files are never pulled from under it).
    /// `Ok(true)` only if both keys were destroyed and the files are gone.
    async fn erase(&self, store_id: &str) -> Result<bool, StoreError> {
        let dir = self.dir(store_id);
        let keys = KeyStore::new(self.keys.slots());
        let id = store_id.to_string();
        tokio::task::spawn_blocking(move || {
            let cache = store::reset(&dir, Kind::Cache, &id, &keys);
            let outbox = store::reset(&dir, Kind::Outbox, &id, &keys);
            let keys_gone = cache? & outbox?; // both attempted before either error returns
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => Ok(keys_gone),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(keys_gone),
                Err(_) => Err(StoreError::Io),
            }
        })
        .await
        .map_err(|_| StoreError::Io)?
    }

    /// At startup, once the index opened and was read cleanly: erase store directories no
    /// index row names (a wipe cut short, or an index whose key was lost).
    pub(crate) async fn reconcile(&self) -> Result<(), StoreError> {
        let doomed: Vec<String> = self
            .index
            .call(|c| {
                c.prepare("SELECT store_id FROM stores WHERE doomed = 1")?
                    .query_map([], |r| r.get(0))?
                    .collect()
            })
            .await?;
        for id in doomed {
            let _ = self.erase_row(&id).await; // still locked: try again next time
        }
        let known: std::collections::HashSet<String> = self
            .index
            .call(|c| {
                c.prepare("SELECT store_id FROM stores")?
                    .query_map([], |r| r.get(0))?
                    .collect()
            })
            .await?;
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(_) => return Err(StoreError::Io),
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();
            // Store ids are 32 hex characters: nothing else here is ours to erase.
            let ours = name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit());
            if kind.is_dir() && ours && !known.contains(&name) {
                if self.orphan_had_unsent(&name).await {
                    self.lost_unsent
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
                self.erase(&name).await?;
            }
        }
        Ok(())
    }

    /// Whether an ownerless store's outbox holds anything unsent. Its own key usually
    /// survives the index's, so it is read; an outbox that can't be read counts as holding
    /// something (saying "lost" wrongly beats losing messages silently).
    async fn orphan_had_unsent(&self, store_id: &str) -> bool {
        self.unsent_in(store_id).await != Some(0)
    }

    /// How many messages a closed store's outbox holds unsent (not yet accepted): `Some(0)`
    /// with no outbox, `None` when it can't be read (its key gone, or not countable).
    async fn unsent_in(&self, store_id: &str) -> Option<u64> {
        let dir = self.dir(store_id);
        if !dir.join("outbox.db").exists() {
            return Some(0);
        }
        let keys = KeyStore::new(self.keys.slots());
        let id = store_id.to_string();
        let opened =
            tokio::task::spawn_blocking(move || store::open(&dir, Kind::Outbox, &id, &keys)).await;
        let (db, rebuilt) = match opened {
            Ok(Ok(Opened::Ready { db, rebuilt })) => (db, rebuilt),
            _ => return None,
        };
        if rebuilt.is_some() {
            // Its key was gone: whatever it held is unreadable (opening made it anew, empty;
            // it's about to be erased either way).
            db.close().await; // closed before the erase that follows, which needs it shut
            return None;
        }
        let count = db
            .call(|c| {
                c.query_row(
                    "SELECT count(*) FROM outbox WHERE state != 'accepted'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
            })
            .await;
        db.close().await;
        count.ok().and_then(|n| u64::try_from(n).ok())
    }

    /// Whether a store with an outbox was erased for want of an owner (then cleared): the app
    /// says once that unsent messages were lost.
    pub(crate) fn take_lost_unsent(&self) -> bool {
        self.lost_unsent
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) async fn close(self) {
        self.index.close().await;
    }
}
