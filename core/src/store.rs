//! Encrypted local stores (plan docs/superpowers/specs/2026-09-25-cache-core-plan.md C1, spec
//! 2026-09-25-offline-cache-design.md §3).
//!
//! Each store is a SQLCipher database with its own random key in a named `KeySlot` slot.
//! Beside it, a non-secret **key check** file holds `HMAC-SHA256(key, CHECK_LABEL)`: the
//! durable answer to "does this file belong to this key?". It decides, across crashes and
//! racing openers, between the only outcomes that matter:
//! - the check matches and the database opens: ready;
//! - the check doesn't match (or is absent) while a database exists: its key is **missing**
//!   (a lost slot, or a crash between making a new key and rebuilding). The old files go and
//!   a fresh store is made, and the caller is told, so an outbox can say what was lost;
//! - the check matches but the database won't open: **damaged**, and nothing is deleted;
//! - the key can't be read: **locked**, nothing is opened and nothing is deleted.
//!
//! A `Connection` lives on one dedicated thread per store (commands over a channel), so no
//! SQLite call runs on an async worker. Closing the channel stops that thread; `close` waits
//! for it, because a wipe must know the file is no longer in use.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, LazyLock, Mutex};

use rusqlite::Connection;
use zeroize::Zeroizing;

use crate::{KeySlot, KeySlotError, KeyStore};

const CHECK_LABEL: &[u8] = b"brook-store-check";

/// Which store, with its file names and schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Cache,
    Outbox,
    /// `(origin, user id) → store id`, so paths never carry a server name or a user.
    Index,
}

impl Kind {
    fn stem(self) -> &'static str {
        match self {
            Kind::Cache => "cache",
            Kind::Outbox => "outbox",
            Kind::Index => "index",
        }
    }

    /// The key slot's name (spec §3: `cache:<store id>`, `outbox:<store id>`).
    fn slot(self, store_id: &str) -> String {
        match self {
            Kind::Index => "index".to_string(),
            _ => format!("{}:{store_id}", self.stem()),
        }
    }

    /// Bumped on any schema change: pre-1.0 there are no migrations (a cache rebuilds; an
    /// outbox is surfaced first).
    fn format(self) -> i64 {
        1
    }

    fn schema(self) -> &'static str {
        match self {
            Kind::Cache => CACHE_V1,
            Kind::Outbox => OUTBOX_V1,
            Kind::Index => INDEX_V1,
        }
    }
}

const CACHE_V1: &str = "
CREATE TABLE meta(id INTEGER PRIMARY KEY CHECK (id = 1), format INTEGER NOT NULL,
                  cursor TEXT NOT NULL DEFAULT '0', generation INTEGER NOT NULL DEFAULT 0);
CREATE TABLE channels(id TEXT PRIMARY KEY, seq INTEGER NOT NULL, json TEXT NOT NULL);
CREATE TABLE removed(channel_id TEXT PRIMARY KEY, seq INTEGER NOT NULL);
CREATE TABLE memberships(channel_id TEXT NOT NULL, user_id TEXT NOT NULL, seq INTEGER NOT NULL,
                         left INTEGER NOT NULL DEFAULT 0, json TEXT NOT NULL,
                         PRIMARY KEY (channel_id, user_id));
CREATE TABLE users(id TEXT PRIMARY KEY, seq INTEGER NOT NULL, json TEXT NOT NULL);
CREATE TABLE messages(id TEXT PRIMARY KEY, channel_id TEXT NOT NULL, seq INTEGER NOT NULL,
                      created_at TEXT NOT NULL, json TEXT NOT NULL);
CREATE INDEX messages_by_channel ON messages(channel_id, id);
CREATE TABLE coverage(channel_id TEXT PRIMARY KEY, newest_id TEXT, oldest_id TEXT,
                      complete_to_start INTEGER NOT NULL DEFAULT 0);
CREATE TABLE files(file_id TEXT PRIMARY KEY, sha256 TEXT NOT NULL, size INTEGER NOT NULL,
                   key BLOB NOT NULL, state TEXT NOT NULL, pinned INTEGER NOT NULL DEFAULT 0,
                   last_opened TEXT);
CREATE TABLE deletions(path TEXT PRIMARY KEY);
";

const OUTBOX_V1: &str = "
CREATE TABLE meta(id INTEGER PRIMARY KEY CHECK (id = 1), format INTEGER NOT NULL,
                  generation INTEGER NOT NULL DEFAULT 0);
CREATE TABLE outbox(ordinal INTEGER PRIMARY KEY AUTOINCREMENT, client_id TEXT NOT NULL UNIQUE,
                    channel_id TEXT NOT NULL, body TEXT NOT NULL, state TEXT NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0, error TEXT, created_at TEXT NOT NULL);
CREATE TABLE outbox_files(client_id TEXT NOT NULL, file_client_id TEXT NOT NULL UNIQUE,
                          snapshot_path TEXT NOT NULL, sha256 TEXT NOT NULL, size INTEGER NOT NULL,
                          content_type TEXT NOT NULL, filename TEXT NOT NULL, file_id TEXT,
                          key BLOB NOT NULL);
";

const INDEX_V1: &str = "
CREATE TABLE meta(id INTEGER PRIMARY KEY CHECK (id = 1), format INTEGER NOT NULL);
CREATE TABLE stores(origin TEXT NOT NULL, user_id TEXT NOT NULL, store_id TEXT NOT NULL UNIQUE,
                    PRIMARY KEY (origin, user_id));
";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum StoreError {
    /// This store is already open in this process (one opener per store).
    #[error("store already open")]
    AlreadyOpen,
    #[error("store I/O failed")]
    Io,
    #[error("store query failed")]
    Sql,
    /// The store's thread stopped (closed, or it died).
    #[error("store closed")]
    Closed,
}

/// Why a store was made fresh over an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rebuilt {
    /// The database's key is gone (the key check didn't match): its contents are lost.
    KeyMissing,
    /// A cache in an older format (pre-1.0: rebuilt, never migrated).
    FormatChanged,
}

pub(crate) enum Opened {
    Ready {
        db: Db,
        rebuilt: Option<Rebuilt>,
    },
    /// The key can't be read now: nothing opened, nothing deleted. Online-only.
    Locked,
    /// The key is right but the database won't open: nothing deleted. Only an explicit
    /// `reset` clears it.
    Damaged,
    /// An outbox in another format: the caller surfaces what will be lost, then `rebuild`s.
    NeedsRebuild,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Opened::Ready { rebuilt, .. } => write!(f, "Ready({rebuilt:?})"),
            Opened::Locked => f.write_str("Locked"),
            Opened::Damaged => f.write_str("Damaged"),
            Opened::NeedsRebuild => f.write_str("NeedsRebuild"),
        }
    }
}

/// The files of one store.
#[derive(Clone)]
struct Paths {
    db: PathBuf,
    check: PathBuf,
}

impl Paths {
    fn new(dir: &Path, kind: Kind) -> Self {
        Self {
            db: dir.join(format!("{}.db", kind.stem())),
            check: dir.join(format!("{}.check", kind.stem())),
        }
    }

    /// The database and every file SQLite may leave beside it.
    fn all(&self) -> Vec<PathBuf> {
        let db = self.db.to_string_lossy().to_string();
        vec![
            self.db.clone(),
            PathBuf::from(format!("{db}-wal")),
            PathBuf::from(format!("{db}-shm")),
            PathBuf::from(format!("{db}-journal")),
            self.check.clone(),
        ]
    }
}

/// Stores open in this process, by database path: two openers of one store would race each
/// other's key decisions (plan C1: "one opener per store").
static OPEN: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);

fn registry() -> std::sync::MutexGuard<'static, HashSet<PathBuf>> {
    OPEN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Open (or make) `kind`'s store in `dir`, keyed by its slot. Blocking: run it off async
/// workers.
pub(crate) fn open(
    dir: &Path,
    kind: Kind,
    store_id: &str,
    keys: &KeyStore<dyn KeySlot>,
) -> Result<Opened, StoreError> {
    let paths = Paths::new(dir, kind);
    if !registry().insert(paths.db.clone()) {
        return Err(StoreError::AlreadyOpen);
    }
    let result = open_registered(dir, kind, store_id, keys, &paths);
    if !matches!(result, Ok(Opened::Ready { .. })) {
        registry().remove(&paths.db); // the thread owns the entry only once it runs
    }
    result
}

fn open_registered(
    dir: &Path,
    kind: Kind,
    store_id: &str,
    keys: &KeyStore<dyn KeySlot>,
    paths: &Paths,
) -> Result<Opened, StoreError> {
    let key = match keys.get_or_create(&kind.slot(store_id)) {
        Ok(key) => key,
        Err(KeySlotError::Exists | KeySlotError::Unavailable | KeySlotError::Fatal(_)) => {
            return Ok(Opened::Locked);
        }
    };
    let check = key_check(key.bytes());
    let mut rebuilt = None;
    if paths.db.exists() {
        let stored = fs::read(&paths.check).ok();
        if stored.as_deref() != Some(check.as_slice()) {
            // Not this key's database: the key it was made with is gone.
            remove_all(paths)?;
            rebuilt = Some(Rebuilt::KeyMissing);
        }
    }
    fs::create_dir_all(dir).map_err(|_| StoreError::Io)?;
    let fresh = !paths.db.exists();
    let conn = match connect(&paths.db, key.bytes()) {
        Ok(conn) => conn,
        Err(_) if !fresh => return Ok(Opened::Damaged),
        Err(_) => return Err(StoreError::Io),
    };
    if fresh {
        conn.execute_batch(kind.schema())
            .map_err(|_| StoreError::Sql)?;
        conn.execute(
            "INSERT INTO meta(id, format) VALUES (1, ?1)",
            [kind.format()],
        )
        .map_err(|_| StoreError::Sql)?;
        // Only now is the database this key's: a crash before this line leaves a database
        // without a check, which the next open treats as keyless and remakes (it was empty).
        write_atomically(&paths.check, &check)?;
    } else {
        let format: rusqlite::Result<i64> =
            conn.query_row("SELECT format FROM meta WHERE id = 1", [], |r| r.get(0));
        match format {
            Ok(f) if f == kind.format() => {}
            Ok(_) if kind == Kind::Outbox => return Ok(Opened::NeedsRebuild),
            Ok(_) => {
                drop(conn);
                remove_all(paths)?;
                return open_registered(dir, kind, store_id, keys, paths).map(|o| match o {
                    Opened::Ready { db, .. } => Opened::Ready {
                        db,
                        rebuilt: Some(Rebuilt::FormatChanged),
                    },
                    other => other,
                });
            }
            // The key matched but the database can't be read: damage, never "absent".
            Err(_) => return Ok(Opened::Damaged),
        }
    }
    Ok(Opened::Ready {
        db: Db::spawn(conn, paths.db.clone()),
        rebuilt,
    })
}

/// Make `kind`'s store fresh after the caller surfaced what is lost (an outbox's format
/// change). The store must not be open.
pub(crate) fn rebuild(
    dir: &Path,
    kind: Kind,
    store_id: &str,
    keys: &KeyStore<dyn KeySlot>,
) -> Result<Opened, StoreError> {
    let paths = Paths::new(dir, kind);
    if registry().contains(&paths.db) {
        return Err(StoreError::AlreadyOpen);
    }
    remove_all(&paths)?;
    open(dir, kind, store_id, keys)
}

/// Erase a store without reading it (a damaged or locked one, or a sign-out wipe once its
/// handles are closed): destroy the key first (crypto-erase), then delete the files. The
/// store must not be open. Returns whether the key was destroyed; the files go either way.
pub(crate) fn reset(
    dir: &Path,
    kind: Kind,
    store_id: &str,
    keys: &KeyStore<dyn KeySlot>,
) -> Result<bool, StoreError> {
    let paths = Paths::new(dir, kind);
    if registry().contains(&paths.db) {
        return Err(StoreError::AlreadyOpen);
    }
    let destroyed = keys.destroy(&kind.slot(store_id)).is_ok();
    remove_all(&paths)?;
    Ok(destroyed)
}

fn connect(path: &Path, key: &[u8; 32]) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    let hex = Zeroizing::new(key.iter().map(|b| format!("{b:02x}")).collect::<String>());
    // Raw key form: no KDF, the key is already random.
    conn.execute_batch(&Zeroizing::new(format!(
        "PRAGMA key = \"x'{}'\";",
        hex.as_str()
    )))?;
    // A wrong key otherwise makes SQLCipher print to stderr by itself.
    conn.execute_batch(
        "PRAGMA cipher_log_level = NONE;
         PRAGMA cipher_memory_security = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA foreign_keys = ON;",
    )?;
    // Reading the journal mode touches page 1: a wrong key or damage fails here.
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if mode != "wal" {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(conn)
}

fn key_check(key: &[u8; 32]) -> Vec<u8> {
    let mac = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    ring::hmac::sign(&mac, CHECK_LABEL).as_ref().to_vec()
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let tmp = path.with_extension("check.tmp");
    let io = |_| StoreError::Io;
    {
        let mut f = fs::File::create(&tmp).map_err(io)?;
        f.write_all(bytes).map_err(io)?;
        f.sync_all().map_err(io)?;
    }
    fs::rename(&tmp, path).map_err(io)?;
    if let Some(dir) = path.parent() {
        fs::File::open(dir).and_then(|d| d.sync_all()).map_err(io)?;
    }
    Ok(())
}

fn remove_all(paths: &Paths) -> Result<(), StoreError> {
    for p in paths.all() {
        match fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(StoreError::Io),
        }
    }
    Ok(())
}

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// An open store: its connection on its own thread.
pub(crate) struct Db {
    jobs: Option<mpsc::Sender<Job>>,
    stopped: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl Db {
    fn spawn(mut conn: Connection, path: PathBuf) -> Self {
        let (jobs, rx) = mpsc::channel::<Job>();
        let (done, stopped) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("brook-store".into())
            .spawn(move || {
                for job in rx {
                    job(&mut conn);
                }
                drop(conn); // closes the database before anyone hears it stopped
                registry().remove(&path);
                let _ = done.send(());
            })
            .expect("spawn the store thread");
        Self {
            jobs: Some(jobs),
            stopped: Some(stopped),
        }
    }

    /// Run `f` on the store's thread.
    pub(crate) async fn call<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let _ = tx.send(f(conn).map_err(|_| StoreError::Sql));
        });
        self.jobs
            .as_ref()
            .ok_or(StoreError::Closed)?
            .send(job)
            .map_err(|_| StoreError::Closed)?;
        rx.await.map_err(|_| StoreError::Closed)?
    }

    /// Stop the thread after the jobs already queued, and wait until the database is closed.
    pub(crate) async fn close(mut self) {
        self.jobs = None;
        if let Some(stopped) = self.stopped.take() {
            let _ = stopped.await;
        }
    }
}

/// The store id for `(origin, user id)`, made (random) on first use. Paths hold only this id.
pub(crate) async fn store_id(
    index: &Db,
    origin: &str,
    user_id: &str,
) -> Result<String, StoreError> {
    let (origin, user_id) = (origin.to_string(), user_id.to_string());
    let mut fresh = [0u8; 16];
    getrandom::fill(&mut fresh).map_err(|_| StoreError::Io)?;
    let candidate: String = fresh.iter().map(|b| format!("{b:02x}")).collect();
    index
        .call(move |conn| {
            let tx = conn.transaction()?;
            let existing: Option<String> = tx
                .query_row(
                    "SELECT store_id FROM stores WHERE origin = ?1 AND user_id = ?2",
                    [&origin, &user_id],
                    |r| r.get(0),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    e => Err(e),
                })?;
            let id = match existing {
                Some(id) => id,
                None => {
                    tx.execute(
                        "INSERT INTO stores(origin, user_id, store_id) VALUES (?1, ?2, ?3)",
                        [&origin, &user_id, &candidate],
                    )?;
                    candidate
                }
            };
            tx.commit()?;
            Ok(id)
        })
        .await
}

/// Whether persistence may put a store on disk at all (plan: one switch, off until C5
/// lands). Every store-opening path asks this first.
pub(crate) fn stores_enabled() -> bool {
    false
}
