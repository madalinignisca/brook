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
    /// The outbox couldn't be kept (its key is gone, or its format changed): unsent messages
    /// on this device were lost. The app says so; the count can't be known.
    pub(crate) outbox_lost: bool,
}

pub(crate) struct LocalData {
    root: PathBuf,
    keys: KeyStore<dyn KeySlot>,
    index: Db,
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
        let local = Self { root, keys, index };
        if rebuilt == Some(Rebuilt::KeyMissing) {
            // The index's key is gone: nobody knows which directory is whose any more, so
            // every store is an orphan. `reconcile` erases them.
            local.reconcile().await?;
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
        let store_id = store::store_id(&self.index, origin, user_id).await?;
        let dir = self.dir(&store_id);
        let keys = KeyStore::new(self.keys.slots());
        let id = store_id.clone();
        let (cache, outbox) = tokio::task::spawn_blocking(move || {
            let cache = store::open(&dir, Kind::Cache, &id, &keys)?;
            let mut outbox = store::open(&dir, Kind::Outbox, &id, &keys)?;
            let mut lost = matches!(
                outbox,
                Opened::Ready {
                    rebuilt: Some(Rebuilt::KeyMissing),
                    ..
                }
            );
            if matches!(outbox, Opened::NeedsRebuild) {
                // A format change: surfaced as lost, then remade.
                lost = true;
                outbox = store::rebuild(&dir, Kind::Outbox, &id, &keys)?;
            }
            Ok::<_, StoreError>((cache, (outbox, lost)))
        })
        .await
        .map_err(|_| StoreError::Io)??;
        let (outbox, outbox_lost) = outbox;
        Ok(UserStores {
            store_id,
            cache,
            outbox,
            outbox_lost,
        })
    }

    /// Erase `(origin, user_id)`'s stores. Their handles must be closed first. The key slots
    /// go first (crypto-erase), then the files, then the index row, so a crash part-way
    /// leaves at worst an orphan that `reconcile` finishes. Works on a locked or damaged
    /// store too: nothing is read.
    pub(crate) async fn wipe(&self, origin: &str, user_id: &str) -> Result<(), StoreError> {
        let (o, u) = (origin.to_string(), user_id.to_string());
        let found: Option<String> = self
            .index
            .call(move |c| {
                use rusqlite::OptionalExtension;
                c.query_row(
                    "SELECT store_id FROM stores WHERE origin = ?1 AND user_id = ?2",
                    [&o, &u],
                    |r| r.get(0),
                )
                .optional()
            })
            .await?;
        let Some(store_id) = found else {
            return Ok(());
        };
        self.erase(&store_id).await?;
        self.index
            .call(move |c| c.execute("DELETE FROM stores WHERE store_id = ?1", [&store_id]))
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

    async fn erase(&self, store_id: &str) -> Result<(), StoreError> {
        let dir = self.dir(store_id);
        let keys = KeyStore::new(self.keys.slots());
        let id = store_id.to_string();
        tokio::task::spawn_blocking(move || {
            store::reset(&dir, Kind::Cache, &id, &keys)?;
            store::reset(&dir, Kind::Outbox, &id, &keys)?;
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(_) => Err(StoreError::Io),
            }
        })
        .await
        .map_err(|_| StoreError::Io)?
    }

    /// At startup, once the index opened and was read cleanly: erase store directories no
    /// index row names (a wipe cut short, or an index whose key was lost).
    pub(crate) async fn reconcile(&self) -> Result<(), StoreError> {
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
                self.erase(&name).await?;
            }
        }
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))] // tests reopen the index
    pub(crate) async fn close(self) {
        self.index.close().await;
    }
}
