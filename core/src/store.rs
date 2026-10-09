// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Local stores (plan docs/superpowers/specs/2026-09-25-cache-core-plan.md C1, spec
//! 2026-09-25-offline-cache-design.md §3).
//!
//! **On Android** a store is plain SQLite in the app's private storage, which the OS
//! encrypts and keeps from other apps (ADR 0001): no key, no `KeySlot` slot, no check file,
//! so nothing here can be "locked" or "missing its key". A plain store that won't open is
//! `Damaged` and kept, and a format change is rebuilt exactly as below. What follows is
//! everywhere else (Linux, macOS, iOS, Windows), where the app's storage is not protected
//! for us and the store protects itself:
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
#[cfg(not(target_os = "android"))]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, LazyLock, Mutex};

use rusqlite::Connection;
#[cfg(not(target_os = "android"))]
use zeroize::Zeroizing;

#[cfg(not(target_os = "android"))]
use crate::KeySlotError;
use crate::{KeySlot, KeyStore};

#[cfg(not(target_os = "android"))]
const CHECK_LABEL: &[u8] = b"brook-store-check";

/// How a store is kept safe at rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Protection {
    /// SQLCipher with a random key in a `KeySlot` slot (everywhere but Android).
    #[cfg(not(target_os = "android"))]
    Keyed,
    /// Plain SQLite: the OS protects the app's storage (Android, ADR 0001). Also built for
    /// tests, so the host can drive this path; no host build uses it otherwise.
    #[cfg(any(target_os = "android", test))]
    Plain,
}

/// The one place the platform picks a `Protection`; `open`, `rebuild` and `reset` use it.
#[cfg(not(target_os = "android"))]
const PROTECTION: Protection = Protection::Keyed;
#[cfg(target_os = "android")]
const PROTECTION: Protection = Protection::Plain;

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
    #[cfg(not(target_os = "android"))]
    fn slot(self, store_id: &str) -> String {
        match self {
            Kind::Index => "index".to_string(),
            _ => format!("{}:{store_id}", self.stem()),
        }
    }

    /// Bumped on any schema change: pre-1.0 there are no migrations (a cache rebuilds; an
    /// outbox is surfaced first).
    fn format(self) -> i64 {
        match self {
            Kind::Cache => 3,  // 2: `removed.active` (the removal floor); 3: the file cache
            Kind::Index => 2,  // 2: `stores.doomed` (a wipe whose keys aren't gone yet)
            Kind::Outbox => 3, // 2: `outbox.reply_to_id`; 3: queued files, `deletions`
        }
    }

    fn schema(self) -> &'static str {
        match self {
            Kind::Cache => CACHE_SCHEMA,
            Kind::Outbox => OUTBOX_V3,
            Kind::Index => INDEX_V1,
        }
    }
}

const CACHE_SCHEMA: &str = "
CREATE TABLE meta(id INTEGER PRIMARY KEY CHECK (id = 1), format INTEGER NOT NULL,
                  cursor TEXT NOT NULL DEFAULT '0', generation INTEGER NOT NULL DEFAULT 0);
CREATE TABLE channels(id TEXT PRIMARY KEY, seq INTEGER NOT NULL, json TEXT NOT NULL);
CREATE TABLE removed(channel_id TEXT PRIMARY KEY, seq INTEGER NOT NULL,
                     active INTEGER NOT NULL DEFAULT 1);
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
                   key BLOB NOT NULL, chunk INTEGER NOT NULL, state TEXT NOT NULL,
                   done INTEGER NOT NULL DEFAULT 0, pinned INTEGER NOT NULL DEFAULT 0,
                   last_used TEXT);
CREATE TABLE message_files(file_id TEXT PRIMARY KEY, message_id TEXT NOT NULL,
                           channel_id TEXT NOT NULL);
CREATE INDEX message_files_by_message ON message_files(message_id);
CREATE INDEX message_files_by_channel ON message_files(channel_id);
CREATE TABLE deletions(path TEXT PRIMARY KEY);
";

const OUTBOX_V3: &str = "
CREATE TABLE meta(id INTEGER PRIMARY KEY CHECK (id = 1), format INTEGER NOT NULL,
                  generation INTEGER NOT NULL DEFAULT 0);
CREATE TABLE outbox(ordinal INTEGER PRIMARY KEY AUTOINCREMENT, client_id TEXT NOT NULL UNIQUE,
                    channel_id TEXT NOT NULL, body TEXT NOT NULL, reply_to_id TEXT,
                    state TEXT NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0, error TEXT, created_at TEXT NOT NULL);
CREATE TABLE outbox_files(client_id TEXT NOT NULL
                              REFERENCES outbox(client_id) ON DELETE CASCADE,
                          ordinal INTEGER NOT NULL, file_client_id TEXT NOT NULL UNIQUE,
                          filename TEXT NOT NULL, content_type TEXT NOT NULL,
                          size INTEGER NOT NULL, sha256 TEXT NOT NULL, key BLOB NOT NULL,
                          chunk INTEGER NOT NULL, file_id TEXT, error TEXT);
CREATE INDEX outbox_files_by_row ON outbox_files(client_id, ordinal);
CREATE TABLE deletions(path TEXT PRIMARY KEY);
";

const INDEX_V1: &str = "
CREATE TABLE meta(id INTEGER PRIMARY KEY CHECK (id = 1), format INTEGER NOT NULL);
CREATE TABLE stores(origin TEXT NOT NULL, user_id TEXT NOT NULL, store_id TEXT NOT NULL UNIQUE,
                    doomed INTEGER NOT NULL DEFAULT 0,
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
    /// The key can't be read now: nothing opened, nothing deleted. Online-only. Not on
    /// Android: a store there has no key to be locked out of.
    #[cfg(not(target_os = "android"))]
    Locked,
    /// The key is right but the database won't open: nothing deleted. Only an explicit
    /// `reset` clears it.
    Damaged,
    /// An outbox in another format: the caller surfaces what will be lost, then `rebuild`s.
    /// `unsent`: its messages not yet accepted by the server, counted before anything is
    /// removed; `None` when they can't be counted (then they count as lost).
    NeedsRebuild {
        unsent: Option<i64>,
    },
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Opened::Ready { rebuilt, .. } => write!(f, "Ready({rebuilt:?})"),
            #[cfg(not(target_os = "android"))]
            Opened::Locked => f.write_str("Locked"),
            Opened::Damaged => f.write_str("Damaged"),
            Opened::NeedsRebuild { unsent } => write!(f, "NeedsRebuild({unsent:?})"),
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

/// A store's registry entry, held for as long as anything opens, rebuilds or erases it.
/// Moved into the store's thread when it opens, and released when that thread ends.
struct Reservation {
    key: PathBuf,
    armed: bool,
}

impl Reservation {
    fn take(key: PathBuf) -> Result<Self, StoreError> {
        if registry().insert(key.clone()) {
            Ok(Self { key, armed: true })
        } else {
            Err(StoreError::AlreadyOpen)
        }
    }

    fn release(&mut self) {
        if std::mem::take(&mut self.armed) {
            registry().remove(&self.key);
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.release();
    }
}

/// `kind`'s files in `dir`, with `dir` made and **canonical**: the registry keys on the real
/// path, so a symlink or `..` can't be a second way into an open store.
fn paths_in(dir: &Path, kind: Kind) -> Result<Paths, StoreError> {
    fs::create_dir_all(dir).map_err(|_| StoreError::Io)?;
    // Owner only: `create_dir_all` follows the umask (usually 0755).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|_| StoreError::Io)?;
    }
    let dir = fs::canonicalize(dir).map_err(|_| StoreError::Io)?;
    Ok(Paths::new(&dir, kind))
}

/// Open (or make) `kind`'s store in `dir`, keyed by its slot. Blocking: run it off async
/// workers.
pub(crate) fn open(
    dir: &Path,
    kind: Kind,
    store_id: &str,
    keys: &KeyStore<dyn KeySlot>,
) -> Result<Opened, StoreError> {
    open_as(PROTECTION, dir, kind, store_id, keys)
}

/// `open` with the protection named, so tests can drive the plain path on the host.
pub(crate) fn open_as(
    protection: Protection,
    dir: &Path,
    kind: Kind,
    store_id: &str,
    keys: &KeyStore<dyn KeySlot>,
) -> Result<Opened, StoreError> {
    let paths = paths_in(dir, kind)?;
    let reservation = Reservation::take(paths.db.clone())?;
    open_reserved(protection, kind, store_id, keys, &paths, reservation)
}

enum Inner {
    Ready(Connection, Option<Rebuilt>),
    Other(Opened),
}

fn open_reserved(
    protection: Protection,
    kind: Kind,
    store_id: &str,
    keys: &KeyStore<dyn KeySlot>,
    paths: &Paths,
    reservation: Reservation,
) -> Result<Opened, StoreError> {
    let mut inner = open_inner(protection, kind, store_id, keys, paths)?;
    if let Inner::Ready(conn, None) = inner {
        // A cache in another format is remade (pre-1.0: no migrations), under the same
        // reservation.
        match format_of(&conn) {
            Ok(f) if f == kind.format() => inner = Inner::Ready(conn, None),
            Ok(_) if kind == Kind::Outbox => {
                // Every outbox format so far has `outbox.state`: count what a rebuild would
                // lose, so an upgrade with nothing waiting reports nothing.
                let unsent = conn
                    .query_row(
                        "SELECT count(*) FROM outbox WHERE state != 'accepted'",
                        [],
                        |r| r.get(0),
                    )
                    .ok();
                return Ok(Opened::NeedsRebuild { unsent });
            }
            Ok(_) => {
                drop(conn);
                remove_all(paths)?;
                inner = match open_inner(protection, kind, store_id, keys, paths)? {
                    Inner::Ready(conn, _) => Inner::Ready(conn, Some(Rebuilt::FormatChanged)),
                    other => other,
                };
            }
            // The key opened it but it can't be read: damage, never "absent".
            Err(_) => return Ok(Opened::Damaged),
        }
    }
    match inner {
        Inner::Ready(conn, rebuilt) => Ok(Opened::Ready {
            db: Db::spawn(conn, reservation)?,
            rebuilt,
        }),
        Inner::Other(opened) => Ok(opened),
    }
}

fn format_of(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT format FROM meta WHERE id = 1", [], |r| r.get(0))
}

fn open_inner(
    protection: Protection,
    kind: Kind,
    store_id: &str,
    keys: &KeyStore<dyn KeySlot>,
    paths: &Paths,
) -> Result<Inner, StoreError> {
    match protection {
        #[cfg(not(target_os = "android"))]
        Protection::Keyed => open_keyed(kind, store_id, keys, paths),
        #[cfg(any(target_os = "android", test))]
        Protection::Plain => {
            // A plain store has no key slot to name or read.
            #[cfg(target_os = "android")]
            let _ = (store_id, keys);
            open_plain(kind, paths)
        }
    }
}

/// A plain store: no key, no check file, `KeyStore` never touched. With nothing to prove
/// whose a file is, a database that won't open is damaged and kept, never deleted; only an
/// explicit `reset` clears it.
#[cfg(any(target_os = "android", test))]
fn open_plain(kind: Kind, paths: &Paths) -> Result<Inner, StoreError> {
    if paths.db.exists() {
        return Ok(match connect_plain(&paths.db) {
            Ok(conn) => Inner::Ready(conn, None),
            Err(_) => Inner::Other(Opened::Damaged),
        });
    }
    let conn = connect_plain(&paths.db).map_err(|_| StoreError::Io)?;
    create_schema(kind, &conn)?;
    Ok(Inner::Ready(conn, None))
}

#[cfg(not(target_os = "android"))]
fn open_keyed(
    kind: Kind,
    store_id: &str,
    keys: &KeyStore<dyn KeySlot>,
    paths: &Paths,
) -> Result<Inner, StoreError> {
    let (key, made_now) = match keys.get_or_create_reporting(&kind.slot(store_id)) {
        Ok(found) => found,
        Err(KeySlotError::Exists | KeySlotError::Unavailable | KeySlotError::Fatal(_)) => {
            return Ok(Inner::Other(Opened::Locked));
        }
    };
    let check = key_check(key.bytes());
    if paths.db.exists() {
        #[derive(PartialEq)]
        enum Check {
            Matches,
            /// Present, for another key: durable proof the database was made with another.
            Other,
            Absent,
        }
        let found = match fs::read(&paths.check) {
            Ok(stored) if stored == check => Check::Matches,
            Ok(_) => Check::Other,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Check::Absent,
            // Unreadable is not absent: nothing is decided, nothing is deleted.
            Err(_) => return Err(StoreError::Io),
        };
        // Only **proof** deletes: a check written for another key, or a key made during this
        // very open (a fresh random key can't be the key of a file already on disk; one
        // opener per store rules out a racing creator). A failed open alone proves nothing:
        // damage, I/O and locking fail the same way. (The probe itself may checkpoint a
        // committed WAL into the file; that moves pages, it loses nothing.)
        let keyless = found == Check::Other || (found == Check::Absent && made_now);
        return Ok(match (found, connect_keyed(&paths.db, key.bytes())) {
            (Check::Matches, Ok(conn)) => Inner::Ready(conn, None),
            // The key opens it: it's this key's, whatever the check said. Restore the check.
            (_, Ok(conn)) => {
                write_atomically(&paths.check, &check)?;
                Inner::Ready(conn, None)
            }
            (_, Err(_)) if keyless => {
                remove_all(paths)?;
                let conn = create(kind, paths, key.bytes(), &check)?;
                Inner::Ready(conn, Some(Rebuilt::KeyMissing))
            }
            // The key should open it and doesn't, or it's unclear whose it is: kept.
            (_, Err(_)) => Inner::Other(Opened::Damaged),
        });
    }
    Ok(Inner::Ready(
        create(kind, paths, key.bytes(), &check)?,
        None,
    ))
}

#[cfg(not(target_os = "android"))]
fn create(
    kind: Kind,
    paths: &Paths,
    key: &[u8; 32],
    check: &[u8],
) -> Result<Connection, StoreError> {
    let conn = connect_keyed(&paths.db, key).map_err(|_| StoreError::Io)?;
    create_schema(kind, &conn)?;
    // A crash before this line leaves a database without a check: the next open finds the
    // key still opens it, and restores the check.
    write_atomically(&paths.check, check)?;
    Ok(conn)
}

/// The schema and the `meta` row of a new database, the same for both protections.
fn create_schema(kind: Kind, conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(kind.schema())
        .map_err(|_| StoreError::Sql)?;
    conn.execute(
        "INSERT INTO meta(id, format) VALUES (1, ?1)",
        [kind.format()],
    )
    .map_err(|_| StoreError::Sql)?;
    Ok(())
}

/// Make `kind`'s store fresh after the caller surfaced what is lost (an outbox's format
/// change). The store must not be open; it stays reserved from the delete to the reopen.
pub(crate) fn rebuild(
    dir: &Path,
    kind: Kind,
    store_id: &str,
    keys: &KeyStore<dyn KeySlot>,
) -> Result<Opened, StoreError> {
    let paths = paths_in(dir, kind)?;
    let reservation = Reservation::take(paths.db.clone())?;
    remove_all(&paths)?;
    open_reserved(PROTECTION, kind, store_id, keys, &paths, reservation)
}

/// Erase a store without reading it (a damaged or locked one, or a sign-out wipe once its
/// handles are closed): destroy the key first (crypto-erase), then delete the files. The
/// store must not be open, and stays reserved throughout. Returns whether the key was
/// destroyed; the files go either way. A plain store has no key: the files go and the
/// answer is `true`, so `LocalData::erase_row`'s "keys gone" condition is met.
pub(crate) fn reset(
    dir: &Path,
    kind: Kind,
    store_id: &str,
    keys: &KeyStore<dyn KeySlot>,
) -> Result<bool, StoreError> {
    reset_as(PROTECTION, dir, kind, store_id, keys)
}

/// `reset` with the protection named, so tests can drive the plain path on the host.
pub(crate) fn reset_as(
    protection: Protection,
    dir: &Path,
    kind: Kind,
    store_id: &str,
    keys: &KeyStore<dyn KeySlot>,
) -> Result<bool, StoreError> {
    let paths = paths_in(dir, kind)?;
    let _reservation = Reservation::take(paths.db.clone())?;
    let destroyed = match protection {
        #[cfg(not(target_os = "android"))]
        Protection::Keyed => keys.destroy(&kind.slot(store_id)).is_ok(),
        #[cfg(any(target_os = "android", test))]
        Protection::Plain => {
            #[cfg(target_os = "android")]
            let _ = (store_id, keys);
            true
        }
    };
    remove_all(&paths)?;
    Ok(destroyed)
}

#[cfg(not(target_os = "android"))]
fn connect_keyed(path: &Path, key: &[u8; 32]) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    // Before the key: a wrong key otherwise makes SQLCipher print to stderr by itself.
    conn.execute_batch("PRAGMA cipher_log_level = NONE;")?;
    let hex = Zeroizing::new(key.iter().map(|b| format!("{b:02x}")).collect::<String>());
    // Raw key form: no KDF, the key is already random.
    conn.execute_batch(&Zeroizing::new(format!(
        "PRAGMA key = \"x'{}'\";",
        hex.as_str()
    )))?;
    conn.execute_batch("PRAGMA cipher_memory_security = ON;")?;
    finish_connect(conn)
}

/// A plain database: opened as it is, with no key pragma at all (the SQLCipher library on
/// the host would encrypt the moment one is set).
#[cfg(any(target_os = "android", test))]
fn connect_plain(path: &Path) -> rusqlite::Result<Connection> {
    finish_connect(Connection::open(path)?)
}

/// What both protections share once the connection is opened (and keyed, if it is).
fn finish_connect(conn: Connection) -> rusqlite::Result<Connection> {
    conn.execute_batch(
        "PRAGMA temp_store = MEMORY;
         PRAGMA foreign_keys = ON;",
    )?;
    // Reading the journal mode touches page 1: a wrong key or damage fails here.
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if mode != "wal" {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(conn)
}

#[cfg(not(target_os = "android"))]
fn key_check(key: &[u8; 32]) -> Vec<u8> {
    let mac = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    ring::hmac::sign(&mac, CHECK_LABEL).as_ref().to_vec()
}

#[cfg(not(target_os = "android"))]
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let tmp = path.with_extension("check.tmp");
    let io = |_| StoreError::Io;
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut f = options.open(&tmp).map_err(io)?;
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
    /// Taken (dropped) by `close`: the thread ends after the jobs already queued.
    jobs: std::sync::Mutex<Option<mpsc::Sender<Job>>>,
    /// Held across the wait by whoever closes first; a second `close` waits for the first.
    stopped: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl Db {
    /// The reservation moves into the thread: if the thread can't start, the closure is
    /// dropped and the reservation with it, so the store isn't left marked open.
    fn spawn(conn: Connection, reservation: Reservation) -> Result<Self, StoreError> {
        let (jobs, rx) = mpsc::channel::<Job>();
        let (done, stopped) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("brook-store".into())
            .spawn(move || {
                // Declared before `conn`, so dropped after it on every exit (a panicking
                // job's unwind included): the database is closed before the store is
                // released and `close` returns.
                let _release = Release {
                    reservation,
                    done: Some(done),
                };
                let mut conn = conn;
                for job in rx {
                    job(&mut conn);
                }
            })
            .map_err(|_| StoreError::Io)?;
        Ok(Self {
            jobs: std::sync::Mutex::new(Some(jobs)),
            stopped: tokio::sync::Mutex::new(Some(stopped)),
        })
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
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .ok_or(StoreError::Closed)?
            .send(job)
            .map_err(|_| StoreError::Closed)?;
        rx.await.map_err(|_| StoreError::Closed)?
    }

    /// Stop the thread after the jobs already queued, and wait until the database is closed.
    /// Through any handle: whoever still holds one gets `Closed` from then on, so nothing
    /// is written after this returns (the guarantee a wipe relies on).
    pub(crate) async fn close(&self) {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        // Awaited in place and cleared only once it fired: a close that is cancelled
        // mid-wait leaves the receiver for the next close to wait on.
        let mut stopped = self.stopped.lock().await;
        if let Some(rx) = stopped.as_mut() {
            let _ = rx.await;
            *stopped = None;
        }
    }
}

/// Releases a store's registry entry (its reservation) and reports it stopped, when its
/// thread ends.
struct Release {
    reservation: Reservation,
    done: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Release {
    fn drop(&mut self) {
        self.reservation.release();
        if let Some(done) = self.done.take() {
            let _ = done.send(());
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

/// Whether local data may be put on disk at all (plan: one switch, off until wipes landed;
/// they did, with C5). Every store-opening path asks this first. It only takes effect where
/// an app calls `enable_local_data` with a durable key store.
pub(crate) fn stores_enabled() -> bool {
    true
}
