//! The file cache (keep-offline spec 2026-09-26, plan core PR 1): attachments downloaded into
//! the signed-in user's store, encrypted at rest, so they open with no connection.
//!
//! - Blobs live at `<store>/files/<file_id>` in the snapshot format (a key per file, 1 MiB
//!   AES-256-GCM chunks), written already sealed as they download (`EncryptingSink`).
//! - A file is named by its id only and looked up in the cached messages (`message_files`):
//!   an id no cached message lists is refused, so every cached row belongs to a message the
//!   cache tracks and leaves with it (`file_rows`).
//! - Downloads run only under the store's own session (`EpochToken`), pause when it ends, and
//!   are single-flight per file: a second caller joins the running one.
//! - Open decrypts into a per-store runtime directory (the one plaintext copy core makes on
//!   its own), refused for executables and launchers.
//! - Unpinned files are evicted by LRU above `FILE_CACHE_CAP`.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{params, OptionalExtension};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::watch;

use crate::cache::{Cache, CacheEvent};
use crate::file_rows;
use crate::snapshot::{self, Layout, Sealer, SnapshotSource, CHUNK};
use crate::store::StoreError;
use crate::transfer::{
    DownloadSink, FileInfo, Flags, SinkError, TransferId, TransferState, Transfers,
};
use crate::Error;

/// Unpinned blobs above this are evicted, least recently used first (spec §5).
pub const FILE_CACHE_CAP: u64 = 1 << 30;

/// Sealed chunks between a blob's fsync and its recorded `done` (the crash-resume point).
const SYNC_EVERY: u64 = 8;

/// After a round with failures, the pin fetcher tries again this much later (or sooner,
/// woken by the connection coming back).
const FETCH_RETRY: Duration = Duration::from_secs(60);

/// The longest a pinned file that keeps failing waits between attempts.
const FETCH_BACKOFF_MAX: Duration = Duration::from_secs(4 * 3600);

/// How often a caller waiting on a shared download checks its own cancel.
const CALLER_TICK: Duration = Duration::from_millis(100);

/// A file as the cache holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileCacheState {
    NotCached,
    /// Downloading, or stopped part way (it resumes).
    Partial {
        done: u64,
        size: u64,
    },
    /// Complete: opens with no connection.
    Cached,
    /// Kept available offline: never evicted, and downloaded whenever there's a connection.
    /// `transfer` is the background download's id while it runs (its progress and cancel).
    Pinned {
        cached: bool,
        done: u64,
        size: u64,
        transfer: Option<TransferId>,
    },
}

/// Where a cached download comes from: the store's own session only (`epoch`).
#[async_trait::async_trait]
pub(crate) trait Download: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn download(
        &self,
        id: TransferId,
        flags: &Arc<Flags>,
        file_id: &str,
        sha256: &str,
        size: u64,
        sink: &mut dyn DownloadSink,
        epoch: u64,
    ) -> crate::Result<()>;
}

/// A download in flight, shared by everyone who asked for this file.
struct Flight {
    /// The download's own flags: set when nobody wants it any more, or on close.
    flags: Arc<Flags>,
    /// The download's own progress id; re-emitted under each caller's.
    id: TransferId,
    /// Callers waiting on it, each with the flags its own id cancels.
    callers: Mutex<HashMap<TransferId, Arc<Flags>>>,
    /// The outcome, once it's over: `None` while running.
    done: watch::Sender<Option<Result<(), Failure>>>,
}

/// A shared download's failure, for every caller (errors aren't `Clone`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Failure {
    Api(String),
    NotAuthenticated,
    Network,
}

impl Failure {
    fn of(err: &Error) -> Self {
        match err {
            Error::Api { code, .. } => Self::Api(code.clone()),
            Error::NotAuthenticated => Self::NotAuthenticated,
            _ => Self::Network,
        }
    }

    fn error(&self) -> Error {
        match self {
            Self::Api(code) => api_error(code),
            Self::NotAuthenticated => Error::NotAuthenticated,
            Self::Network => api_error("transfer.network"),
        }
    }
}

/// The server wasn't reached (or there's no session): not the file's fault, so it doesn't
/// back the file off. A failure the server answered (a 5xx, wrong bytes) does.
pub(crate) fn is_connection_failure(err: &Error) -> bool {
    match err {
        Error::Api { code, .. } => matches!(code.as_str(), "transfer.network" | "transfer.paused"),
        Error::NotAuthenticated | Error::Http(_) | Error::Timeout | Error::Disconnected => true,
        _ => false,
    }
}

fn api_error(code: &str) -> Error {
    let message = match code {
        "file.unknown" => "no cached message has this file",
        "file.gone" => "the file was deleted",
        "file.open_refused" => "this kind of file is only saved, never opened",
        "transfer.cancelled" => "the transfer was cancelled",
        "local.unavailable" => "offline storage isn't available",
        _ => "the file couldn't be cached",
    };
    Error::Api {
        code: code.into(),
        message: message.into(),
    }
}

fn store_failed(_: StoreError) -> Error {
    api_error("local.store")
}

/// A cached file's row.
#[derive(Debug, Clone)]
struct Row {
    key: [u8; 32],
    chunk: usize,
    size: u64,
    sha256: String,
    state: String,
    done: u64,
    pinned: bool,
}

pub(crate) struct Files {
    cache: Arc<Cache>,
    store_dir: PathBuf,
    blobs: PathBuf,
    /// Where Open puts its plaintext copies; `None` where there's no private runtime dir.
    open_dir: Option<PathBuf>,
    net: Arc<dyn Download>,
    transfers: Arc<Transfers>,
    session: watch::Receiver<Option<u64>>,
    flights: Mutex<HashMap<String, Arc<Flight>>>,
    /// Download tasks, joined on close.
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Unlinks journalled blobs after the cache drops files; aborted on close.
    sweeper: Mutex<Option<tokio::task::JoinHandle<()>>>,
    closed: AtomicBool,
    /// `FILE_CACHE_CAP`, lowered by tests.
    cap: std::sync::atomic::AtomicU64,
    /// Held by the journal sweep and by whoever makes a row and opens its blob: a sweep never
    /// unlinks a blob that a new download of the same file has just opened.
    blob_lock: tokio::sync::Mutex<()>,
    /// Wakes the pin fetcher (a pin, the connection back, the session back).
    fetch_wake: Arc<tokio::sync::Notify>,
    /// Pinned files the fetcher is downloading now, with the id their progress runs under.
    fetching: Mutex<HashMap<String, TransferId>>,
    /// How long the fetcher waits after a failed round before trying again (tests shorten it).
    fetch_retry: Mutex<Duration>,
    /// Pinned files that keep failing: how many times in a row, and not before when. The
    /// wait doubles each time (from `fetch_retry`, up to `FETCH_BACKOFF_MAX`), so a file that
    /// can't be fetched (a server that keeps sending wrong bytes) isn't re-downloaded whole
    /// every minute. Reset by a success or a new pin.
    fetch_failures: Mutex<HashMap<String, (u32, tokio::time::Instant)>>,
}

impl Files {
    #[cfg(test)]
    pub(crate) fn set_cap(&self, cap: u64) {
        self.cap.store(cap, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn set_fetch_retry(&self, wait: Duration) {
        *lock(&self.fetch_retry) = wait;
        self.fetch_wake.notify_one();
    }

    // ---- Keep available offline ----

    /// Keep `file_id` on this device: never evicted, downloaded now or as soon as there's a
    /// connection (and again after a restart until it's complete). Durable.
    pub(crate) async fn pin_file(&self, file_id: &str) -> crate::Result<()> {
        self.check_open()?;
        let info = self.lookup(file_id).await?;
        let sha256 = info
            .sha256
            .clone()
            .ok_or_else(|| api_error("file.unknown"))?;
        let key = new_key()?;
        let (fid, k, size) = (file_id.to_string(), key.to_vec(), info.size as i64);
        let made = self
            .cache
            .db()
            .call(move |c| {
                c.execute(
                    "INSERT INTO files(file_id, sha256, size, key, chunk, state, done, pinned)
                     SELECT ?1, ?2, ?3, ?4, ?5, 'partial', 0, 1
                     WHERE EXISTS (SELECT 1 FROM message_files WHERE file_id = ?1)
                     ON CONFLICT(file_id) DO UPDATE SET pinned = 1",
                    params![fid, sha256, size, k, CHUNK as i64],
                )
            })
            .await
            .map_err(store_failed)?;
        if made == 0 {
            return Err(api_error("file.gone"));
        }
        lock(&self.fetch_failures).remove(file_id); // pinning again retries at once
        self.cache
            .announce(CacheEvent::Files(vec![file_id.to_string()]));
        self.fetch_wake.notify_one();
        Ok(())
    }

    /// Stop keeping `file_id`: it stays cached as an ordinary file (evictable). A background
    /// download of it stops unless someone else is waiting on it (an Open).
    pub(crate) async fn unpin_file(&self, file_id: &str) -> crate::Result<()> {
        self.check_open()?;
        let fid = file_id.to_string();
        self.cache
            .db()
            .call(move |c| c.execute("UPDATE files SET pinned = 0 WHERE file_id = ?1", [&fid]))
            .await
            .map_err(store_failed)?;
        let fetching = lock(&self.fetching).get(file_id).copied();
        if let Some(id) = fetching {
            self.transfers.flag(id).cancel.store(true, Ordering::SeqCst);
        }
        self.cache
            .announce(CacheEvent::Files(vec![file_id.to_string()]));
        Ok(())
    }

    /// The size of every pinned file (they don't count against the cap).
    pub(crate) async fn pinned_bytes(&self) -> crate::Result<u64> {
        self.cache
            .db()
            .call(|c| {
                c.query_row(
                    "SELECT COALESCE(SUM(size), 0) FROM files WHERE pinned = 1",
                    [],
                    |r| r.get::<_, i64>(0),
                )
            })
            .await
            .map(|n| n as u64)
            .map_err(store_failed)
    }

    /// The background task that downloads pinned files, one at a time: at open, on a pin,
    /// when the connection or the session comes back, and a while after a failed round.
    fn start_fetcher(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let wake = self.fetch_wake.clone();
        let mut online = self.cache.state();
        let mut session = self.session.clone();
        let task = tokio::spawn(async move {
            loop {
                let Some(files) = weak.upgrade() else { break };
                if files.closed.load(Ordering::SeqCst) {
                    break;
                }
                let failed = files.fetch_round().await;
                let retry = *lock(&files.fetch_retry);
                drop(files);
                let mut was_offline = online.borrow_and_update().offline;
                // Wait for a reason to go round again.
                loop {
                    tokio::select! {
                        _ = wake.notified() => break,
                        changed = online.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            let now_offline = online.borrow_and_update().offline;
                            // Only the way back online matters.
                            if was_offline && !now_offline {
                                // A round now: pins that failed for want of a connection have
                                // no backoff and go at once. Backoffs from failures the server
                                // answered keep their wait (a flapping signal mustn't re-download
                                // a stuck file on every reconnect).
                                break;
                            }
                            was_offline = now_offline;
                        }
                        changed = session.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            if session.borrow_and_update().is_some() {
                                break;
                            }
                        }
                        _ = tokio::time::sleep(retry), if failed => break,
                    }
                }
            }
        });
        lock(&self.tasks).push(task);
    }

    /// Download every pinned file that isn't complete; whether any failed.
    async fn fetch_round(self: &Arc<Self>) -> bool {
        if self.closed.load(Ordering::SeqCst) || self.session.borrow().is_none() {
            return false;
        }
        let wanted = self
            .cache
            .db()
            .call(|c| {
                c.prepare("SELECT file_id FROM files WHERE pinned = 1 AND state != 'complete'")?
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()
            })
            .await
            .unwrap_or_default();
        let mut failed = false;
        let now = tokio::time::Instant::now();
        for file_id in wanted {
            if self.closed.load(Ordering::SeqCst) {
                break;
            }
            // Backing off after failures: not yet (a later round picks it up).
            let due = lock(&self.fetch_failures)
                .get(&file_id)
                .is_none_or(|(_, not_before)| *not_before <= now);
            if !due {
                failed = true; // keeps the fetcher coming back
                continue;
            }
            let id = TransferId::new();
            lock(&self.fetching).insert(file_id.clone(), id);
            self.cache
                .announce(CacheEvent::Files(vec![file_id.clone()]));
            let result = self.cache_file(id, &file_id).await;
            lock(&self.fetching).remove(&file_id);
            match &result {
                Ok(()) => {
                    lock(&self.fetch_failures).remove(&file_id);
                }
                // Gone or unknown: the lifecycle dropped it, pin and all. Cancelled: unpinned
                // (or the store is closing).
                Err(Error::Api { code, .. })
                    if matches!(
                        code.as_str(),
                        "file.gone" | "file.unknown" | "transfer.cancelled"
                    ) => {}
                // No connection (or no session): not the file's fault, so no backoff for it;
                // the connection or the session coming back wakes the fetcher.
                Err(e) if is_connection_failure(e) => failed = true,
                Err(_) => {
                    failed = true;
                    let base = *lock(&self.fetch_retry);
                    let mut failures = lock(&self.fetch_failures);
                    let n = failures.get(&file_id).map_or(0, |(n, _)| *n) + 1;
                    let wait = base
                        .saturating_mul(1u32 << (n - 1).min(16))
                        .min(FETCH_BACKOFF_MAX);
                    failures.insert(file_id.clone(), (n, tokio::time::Instant::now() + wait));
                }
            }
        }
        failed
    }

    /// Open the file cache of the store at `store_dir` and reconcile it (best-effort, before
    /// anything uses it): the journal is finished, blobs no row names are deleted, partials
    /// whose blob is short are dropped, and old Open copies are removed.
    pub(crate) async fn open(
        cache: Arc<Cache>,
        store_dir: &Path,
        open_dir: Option<PathBuf>,
        net: Arc<dyn Download>,
        transfers: Arc<Transfers>,
        session: watch::Receiver<Option<u64>>,
    ) -> Arc<Self> {
        let blobs = store_dir.join("files");
        let _ = std::fs::create_dir(&blobs); // under the store dir (0700); never recursive
        let files = Arc::new(Self {
            cache,
            store_dir: store_dir.to_path_buf(),
            blobs,
            open_dir,
            cap: std::sync::atomic::AtomicU64::new(FILE_CACHE_CAP),
            blob_lock: tokio::sync::Mutex::new(()),
            fetch_wake: Arc::default(),
            fetching: Mutex::default(),
            fetch_retry: Mutex::new(FETCH_RETRY),
            fetch_failures: Mutex::default(),
            net,
            transfers,
            session,
            flights: Mutex::default(),
            tasks: Mutex::default(),
            sweeper: Mutex::default(),
            closed: AtomicBool::new(false),
        });
        files.reconcile().await;
        // Unlink journalled blobs whenever the cache drops files (after its commit).
        let sweeper = {
            let (weak, notify) = (Arc::downgrade(&files), files.cache.files_dropped.clone());
            tokio::spawn(async move {
                loop {
                    notify.notified().await;
                    let Some(files) = weak.upgrade() else { break };
                    files.sweep_journal().await;
                    files.cancel_unlisted().await;
                }
            })
        };
        *lock(&files.sweeper) = Some(sweeper);
        files.start_fetcher();
        files
    }

    /// Stop: new calls fail, every download in flight is cancelled and **joined** (its
    /// partial kept), so nothing writes into the store once this returns. Called before the
    /// cache and the outbox close, and so before any wipe erases the store.
    pub(crate) async fn close(&self) {
        {
            let flights = lock(&self.flights);
            self.closed.store(true, Ordering::SeqCst);
            for flight in flights.values() {
                flight.flags.cancel.store(true, Ordering::SeqCst);
            }
        }
        self.fetch_wake.notify_one(); // the fetcher sees `closed` and ends
        let sweeper = lock(&self.sweeper).take();
        if let Some(sweeper) = sweeper {
            sweeper.abort();
            let _ = sweeper.await;
        }
        // Downloads end at their next chunk, or out of a backoff (checked every 100 ms);
        // each records its partial before its task ends.
        let tasks: Vec<_> = lock(&self.tasks).drain(..).collect();
        for task in tasks {
            if tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .is_err()
            {
                tracing::warn!("a file download didn't stop in time");
            }
        }
    }

    fn check_open(&self) -> crate::Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            Err(api_error("local.unavailable"))
        } else {
            Ok(())
        }
    }

    // ---- The public operations (through `BrookClient`) ----

    /// Download `file_id` into the cache (resuming a partial), or join the download already
    /// running. Progress and cancel under `id`.
    pub(crate) async fn cache_file(
        self: &Arc<Self>,
        id: TransferId,
        file_id: &str,
    ) -> crate::Result<()> {
        self.check_open()?;
        let info = self.lookup(file_id).await?;
        if self
            .row(file_id)
            .await?
            .is_some_and(|r| r.state == "complete")
        {
            self.touch(file_id).await;
            self.transfers
                .emit(id, info.size, info.size, TransferState::Done);
            return Ok(());
        }
        let caller = Arc::new(Flags::default());
        self.transfers.register(&[id], &caller);
        let mut result = Err(api_error("transfer.cancelled"));
        for _ in 0..3 {
            let flight = match self.join_or_start(file_id, &info, id, &caller) {
                Ok(f) => f,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            };
            result = self.wait(&flight, id, &caller).await;
            // It met a download that was stopping (every earlier caller cancelled): this
            // caller didn't, so it starts a new one once that ends.
            let stopped_for_others = matches!(&result, Err(Error::Api { code, .. }) if code == "transfer.cancelled")
                && !caller.cancel.load(Ordering::SeqCst)
                && !self.closed.load(Ordering::SeqCst);
            if !stopped_for_others {
                break;
            }
        }
        self.transfers.unregister(&[id]);
        let state = match &result {
            Ok(()) => TransferState::Done,
            Err(Error::Api { code, .. }) if code == "transfer.cancelled" => {
                TransferState::Cancelled
            }
            Err(err) => TransferState::Failed(match Failure::of(err) {
                Failure::Api(code) => code,
                Failure::NotAuthenticated => "auth.not_authenticated".into(),
                Failure::Network => "transfer.network".into(),
            }),
        };
        let done = if result.is_ok() { info.size } else { 0 };
        self.transfers.emit(id, done, info.size, state);
        if result.is_ok() {
            self.touch(file_id).await;
        }
        result
    }

    /// Cache `file_id`, then decrypt it into this store's Open directory under its safe name,
    /// in a fresh subdirectory. Refused for executables and launchers (sniffed, never by name).
    pub(crate) async fn open_file(
        self: &Arc<Self>,
        id: TransferId,
        file_id: &str,
    ) -> crate::Result<PathBuf> {
        let open_dir = self
            .open_dir
            .clone()
            .ok_or_else(|| api_error("local.unavailable"))?;
        self.cache_file(id, file_id).await?;
        let info = self.lookup(file_id).await?;
        let row = self
            .row(file_id)
            .await?
            .ok_or_else(|| api_error("file.gone"))?;
        let source = self.source(file_id, &row)?;
        let mut reader = source_reader(&source).await?;
        let mut head = vec![0u8; SNIFF];
        let n = read_up_to(&mut reader, &mut head).await?;
        head.truncate(n);
        // Judged on the very name the copy gets.
        let leaf = safe_leaf(&info.filename);
        if !openable(&leaf, &head) {
            return Err(api_error("file.open_refused"));
        }
        let dir = open_dir.join(random_hex());
        create_private_dir(&dir).map_err(|e| io_err(&e))?;
        let path = dir.join(&leaf);
        let written = async {
            let mut out = private_file(&path).await?;
            out.write_all(&head).await?;
            tokio::io::copy(&mut reader, &mut out).await?;
            out.flush().await?;
            out.sync_all().await
        }
        .await;
        if let Err(e) = written {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(io_err(&e));
        }
        mark_downloaded(&path);
        self.touch(file_id).await;
        Ok(path)
    }

    /// Save `file_id` to `destination`: decrypted from the cache when it's complete there
    /// (so it works offline), else `None` (the caller downloads it straight there, as
    /// before: Save isn't Open, it doesn't cache). `destination` is truncated first and removed
    /// on failure: to replace a file, pass a temporary path and rename it over the file
    /// afterwards (GTK's `.brook-part`, the Mac's `replaceItemAt`).
    pub(crate) async fn save_from_cache(
        &self,
        file_id: &str,
        destination: &Path,
    ) -> crate::Result<Option<()>> {
        self.check_open()?;
        let info = self.lookup(file_id).await?;
        let Some(row) = self.row(file_id).await?.filter(|r| r.state == "complete") else {
            return Ok(None);
        };
        let source = self.source(file_id, &row)?;
        let result = async {
            let mut reader = source_reader(&source)
                .await
                .map_err(|_| io::ErrorKind::InvalidData)?;
            let mut out = tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(destination)
                .await?;
            let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = reader.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                digest.update(&buf[..n]);
                out.write_all(&buf[..n]).await?;
            }
            out.flush().await?;
            out.sync_all().await?;
            let got: String = digest
                .finish()
                .as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if Some(got) != info.sha256 {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                self.touch(file_id).await;
                Ok(Some(()))
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(destination).await; // never a wrong file left
                Err(if e.kind() == io::ErrorKind::InvalidData {
                    api_error("transfer.integrity")
                } else {
                    io_err(&e)
                })
            }
        }
    }

    pub(crate) async fn state(&self, file_id: &str) -> crate::Result<FileCacheState> {
        Ok(match self.row(file_id).await? {
            None => FileCacheState::NotCached,
            Some(r) if r.pinned => FileCacheState::Pinned {
                cached: r.state == "complete",
                done: if r.state == "complete" {
                    r.size
                } else {
                    r.done
                },
                size: r.size,
                transfer: lock(&self.fetching).get(file_id).copied(),
            },
            Some(r) if r.state == "complete" => FileCacheState::Cached,
            Some(r) => FileCacheState::Partial {
                done: r.done,
                size: r.size,
            },
        })
    }

    /// Remove this store's Open copies (the app calls this on quit; best-effort: the sweep
    /// at the next open is the guarantee).
    pub(crate) fn clear_open_copies(&self) {
        if let Some(dir) = &self.open_dir {
            clear_dir(dir);
        }
    }

    // ---- Downloads ----

    fn join_or_start(
        self: &Arc<Self>,
        file_id: &str,
        info: &FileInfo,
        caller_id: TransferId,
        caller: &Arc<Flags>,
    ) -> crate::Result<Arc<Flight>> {
        let mut flights = lock(&self.flights);
        // Under the same lock `close` sets `closed` with: no download starts once close has
        // begun, so every one it must join is already in `tasks`.
        self.check_open()?;
        if let Some(flight) = flights.get(file_id) {
            // One that's stopping (every caller cancelled) isn't joined: the caller waits
            // it out and starts again (see `cache_file`).
            if flight.flags.cancel.load(Ordering::SeqCst) {
                return Ok(flight.clone());
            }
            lock(&flight.callers).insert(caller_id, caller.clone());
            return Ok(flight.clone());
        }
        let (done, _) = watch::channel(None);
        let flight = Arc::new(Flight {
            flags: Arc::new(Flags::default()),
            id: TransferId::new(),
            callers: Mutex::new(HashMap::from([(caller_id, caller.clone())])),
            done,
        });
        flights.insert(file_id.to_string(), flight.clone());
        let (me, file_id, info, f) = (
            self.clone(),
            file_id.to_string(),
            info.clone(),
            flight.clone(),
        );
        let task = tokio::spawn(async move {
            let result = me.run(&file_id, &info, &f).await;
            lock(&me.flights).remove(&file_id);
            f.done
                .send_replace(Some(result.map_err(|e| Failure::of(&e))));
        });
        lock(&self.tasks).push(task);
        Ok(flight)
    }

    /// Wait for a shared download as one caller: its own cancel detaches it, and the
    /// download stops once nobody waits on it any more.
    async fn wait(
        &self,
        flight: &Arc<Flight>,
        id: TransferId,
        caller: &Arc<Flags>,
    ) -> crate::Result<()> {
        let mut done = flight.done.subscribe();
        let mut progress = self.transfers.subscribe();
        loop {
            if let Some(result) = done.borrow_and_update().clone() {
                return result.map_err(|f| f.error());
            }
            if caller.cancel.load(Ordering::SeqCst) {
                let mut callers = lock(&flight.callers);
                callers.remove(&id);
                if callers.is_empty() {
                    flight.flags.cancel.store(true, Ordering::SeqCst);
                }
                return Err(api_error("transfer.cancelled"));
            }
            tokio::select! {
                _ = done.changed() => {}
                event = progress.recv() => {
                    // The download's own progress, under this caller's id.
                    if let Ok(e) = event {
                        if e.id == flight.id
                            && matches!(e.state, TransferState::Running | TransferState::Retrying { .. })
                        {
                            self.transfers.emit(id, e.done, e.total, e.state);
                        }
                    }
                }
                _ = tokio::time::sleep(CALLER_TICK) => {}
            }
        }
    }

    /// One download into the cache, under the store's session.
    async fn run(
        self: &Arc<Self>,
        file_id: &str,
        info: &FileInfo,
        flight: &Arc<Flight>,
    ) -> crate::Result<()> {
        let sha256 = info
            .sha256
            .clone()
            .ok_or_else(|| api_error("file.unknown"))?;
        let epoch = (*self.session.borrow()).ok_or(Error::NotAuthenticated)?;
        // Any change away from this session pauses the download at its next chunk.
        let pauser = {
            let (flags, mut session) = (flight.flags.clone(), self.session.clone());
            AbortOnDrop(tokio::spawn(async move {
                while *session.borrow_and_update() == Some(epoch) {
                    if session.changed().await.is_err() {
                        break;
                    }
                }
                flags.pause.store(true, Ordering::SeqCst);
            }))
        };
        let mut sink = EncryptingSink::open(self.clone(), file_id, info.size, &sha256).await?;
        let result = self
            .net
            .download(
                flight.id,
                &flight.flags,
                file_id,
                &sha256,
                info.size,
                &mut sink,
                epoch,
            )
            .await;
        drop(pauser);
        match &result {
            Ok(()) => self.evict(file_id).await,
            Err(Error::Api { code, .. }) if code == "file.gone" => {
                sink.abort().await;
                self.drop_file(file_id).await;
            }
            Err(_) => sink.abort().await, // the partial stays for a later resume
        }
        result
    }

    /// The server deleted the file: drop it as the cache's lifecycle would.
    async fn drop_file(&self, file_id: &str) {
        let id = file_id.to_string();
        let dropped = self
            .cache
            .db()
            .call(move |c| {
                let tx = c.transaction()?;
                tx.execute("DELETE FROM message_files WHERE file_id = ?1", [&id])?;
                let dropped = file_rows::drop_files(&tx, std::slice::from_ref(&id))?;
                tx.commit()?;
                Ok(dropped)
            })
            .await;
        if let Ok(ids) = dropped {
            self.cache.announce(CacheEvent::Files(ids));
        }
        self.sweep_journal().await;
    }

    // ---- Eviction and the journal ----

    /// Keep unpinned blobs under the cap: partials first, then the least recently used.
    /// Never the file just finished, nor one downloading.
    async fn evict(&self, just: &str) {
        let cap = self.cap.load(Ordering::SeqCst);
        let mut keep: HashSet<String> = lock(&self.flights).keys().cloned().collect();
        keep.insert(just.to_string());
        let dropped = self
            .cache
            .db()
            .call(move |c| {
                let tx = c.transaction()?;
                // A partial counts at its full size: it will be that big (and the sum stays
                // a plain SUM).
                let mut total: u64 = tx.query_row(
                    "SELECT COALESCE(SUM(size), 0) FROM files WHERE pinned = 0",
                    [],
                    |r| r.get::<_, i64>(0),
                )? as u64;
                let mut gone = Vec::new();
                if total > cap {
                    let candidates: Vec<(String, i64)> = tx
                        .prepare(
                            "SELECT file_id, size FROM files WHERE pinned = 0
                             ORDER BY (state = 'partial') DESC, COALESCE(last_used, '') ASC",
                        )?
                        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<rusqlite::Result<_>>()?;
                    for (id, size) in candidates {
                        if total <= cap {
                            break;
                        }
                        if keep.contains(&id) {
                            continue;
                        }
                        gone.extend(file_rows::drop_files(&tx, &[id])?);
                        total = total.saturating_sub(size as u64);
                    }
                }
                tx.commit()?;
                Ok(gone)
            })
            .await;
        if let Ok(ids) = dropped {
            if !ids.is_empty() {
                // Evicted, not gone: the message still lists them (they download again).
                self.cache.announce(CacheEvent::Files(ids));
                self.sweep_journal().await;
            }
        }
    }

    /// Unlink the journalled blobs, then forget them (NotFound counts as done).
    pub(crate) async fn sweep_journal(&self) {
        let _blobs = self.blob_lock.lock().await;
        // A path whose file has a row again (evicted, then downloaded anew) is live: its entry
        // is stale and goes, the blob stays.
        let Ok(paths) = self
            .cache
            .db()
            .call(|c| {
                let tx = c.transaction()?;
                tx.execute(
                    "DELETE FROM deletions WHERE EXISTS
                         (SELECT 1 FROM files WHERE 'files/' || files.file_id = deletions.path)",
                    [],
                )?;
                let paths = tx
                    .prepare("SELECT path FROM deletions")?
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                tx.commit()?;
                Ok(paths)
            })
            .await
        else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        let dir = self.store_dir.clone();
        let removed = tokio::task::spawn_blocking(move || {
            paths
                .into_iter()
                .filter(|p| match std::fs::remove_file(dir.join(p)) {
                    Ok(()) => true,
                    Err(e) => e.kind() == io::ErrorKind::NotFound,
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        let _ = self
            .cache
            .db()
            .call(move |c| {
                for p in &removed {
                    c.execute("DELETE FROM deletions WHERE path = ?1", [p])?;
                }
                Ok(())
            })
            .await;
    }

    /// Downloads of files no cached message lists any more stop (their rows are gone).
    async fn cancel_unlisted(&self) {
        let flying: Vec<String> = lock(&self.flights).keys().cloned().collect();
        if flying.is_empty() {
            return;
        }
        let listed = self
            .cache
            .db()
            .call(move |c| {
                let mut listed = HashSet::new();
                for id in &flying {
                    let hit: Option<i64> = c
                        .query_row(
                            "SELECT 1 FROM message_files WHERE file_id = ?1",
                            [id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    if hit.is_some() {
                        listed.insert(id.clone());
                    }
                }
                Ok(listed)
            })
            .await
            .unwrap_or_default();
        for (id, flight) in lock(&self.flights).iter() {
            if !listed.contains(id) {
                flight.flags.cancel.store(true, Ordering::SeqCst);
            }
        }
    }

    async fn reconcile(&self) {
        self.sweep_journal().await;
        // Rows as recorded; blobs as found.
        let rows = self
            .cache
            .db()
            .call(|c| {
                c.prepare("SELECT file_id, state, size, chunk, done FROM files")?
                    .query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
            .unwrap_or_default();
        let blobs = self.blobs.clone();
        let named: HashMap<String, (String, u64, usize, u64)> = rows
            .into_iter()
            .map(|(id, state, size, chunk, done)| {
                (id, (state, size as u64, chunk as usize, done as u64))
            })
            .collect();
        let bad = tokio::task::spawn_blocking(move || {
            // A blob no row names goes.
            if let Ok(entries) = std::fs::read_dir(&blobs) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !named.contains_key(&name) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            // A row whose blob is shorter than it records (or missing, when it records any
            // bytes) can't be trusted.
            named
                .into_iter()
                .filter(|(id, (state, size, chunk, done))| {
                    let layout = Layout {
                        chunk: *chunk,
                        size: *size,
                    };
                    let need = if state == "complete" {
                        layout.file_len()
                    } else {
                        layout.sealed_offset(done / *chunk as u64)
                    };
                    // Nothing downloaded yet (a pin waiting for a connection): no blob is
                    // fine, the first download makes it.
                    if need == 0 {
                        return false;
                    }
                    let have = std::fs::metadata(blobs.join(id)).map(|m| m.len()).ok();
                    have.is_none_or(|h| h < need)
                })
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        if !bad.is_empty() {
            let _ = self
                .cache
                .db()
                .call(move |c| {
                    let tx = c.transaction()?;
                    for id in &bad {
                        let pinned: bool = tx.query_row(
                            "SELECT pinned FROM files WHERE file_id = ?1",
                            [id],
                            |r| r.get::<_, i64>(0).map(|p| p != 0),
                        )?;
                        if pinned {
                            // Kept offline: start it over (a new key, from 0), keeping the
                            // pin. The next download truncates the blob to 0 as it opens it.
                            let mut key = [0u8; 32];
                            let _ = getrandom::fill(&mut key);
                            tx.execute(
                                "UPDATE files SET key = ?2, done = 0, state = 'partial'
                                 WHERE file_id = ?1",
                                params![id, key.to_vec()],
                            )?;
                        } else {
                            file_rows::drop_files(&tx, std::slice::from_ref(id))?;
                        }
                    }
                    tx.commit()
                })
                .await;
            self.sweep_journal().await;
        }
        self.clear_open_copies();
    }

    // ---- Rows ----

    /// The file as its cached message lists it; `file.unknown` if none does.
    async fn lookup(&self, file_id: &str) -> crate::Result<FileInfo> {
        let id = file_id.to_string();
        let json: Option<String> = self
            .cache
            .db()
            .call(move |c| {
                c.query_row(
                    "SELECT m.json FROM message_files f JOIN messages m ON m.id = f.message_id
                     WHERE f.file_id = ?1",
                    [&id],
                    |r| r.get(0),
                )
                .optional()
            })
            .await
            .map_err(store_failed)?;
        let json: serde_json::Value = json
            .and_then(|j| serde_json::from_str(&j).ok())
            .ok_or_else(|| api_error("file.unknown"))?;
        json.get("attachments")
            .and_then(|a| a.as_array())
            .and_then(|a| {
                a.iter()
                    .find(|f| f.get("id").and_then(|v| v.as_str()) == Some(file_id))
            })
            .and_then(|f| serde_json::from_value::<FileInfo>(f.clone()).ok())
            .filter(|f| f.sha256.is_some() && f.size > 0)
            .ok_or_else(|| api_error("file.unknown"))
    }

    async fn row(&self, file_id: &str) -> crate::Result<Option<Row>> {
        let id = file_id.to_string();
        self.cache
            .db()
            .call(move |c| {
                c.query_row(
                    "SELECT key, chunk, size, sha256, state, done, pinned FROM files
                     WHERE file_id = ?1",
                    [&id],
                    |r| {
                        let key: Vec<u8> = r.get(0)?;
                        Ok((
                            key,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get::<_, i64>(5)?,
                            r.get::<_, i64>(6)? != 0,
                        ))
                    },
                )
                .optional()
            })
            .await
            .map_err(store_failed)
            .map(|found| {
                found.and_then(|(key, chunk, size, sha256, state, done, pinned)| {
                    Some(Row {
                        key: key.try_into().ok()?,
                        chunk: chunk as usize,
                        size: size as u64,
                        sha256,
                        state,
                        done: done as u64,
                        pinned,
                    })
                })
            })
    }

    async fn touch(&self, file_id: &str) {
        let id = file_id.to_string();
        let _ = self
            .cache
            .db()
            .call(move |c| {
                c.execute(
                    "UPDATE files SET last_used = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                     WHERE file_id = ?1",
                    [&id],
                )
            })
            .await;
    }

    fn source(&self, file_id: &str, row: &Row) -> crate::Result<SnapshotSource> {
        Ok(SnapshotSource {
            path: self.blobs.join(file_id),
            key: row.key,
            id: snapshot::id_bytes(file_id).ok_or_else(|| api_error("file.unknown"))?,
            size: row.size,
            sha256: row.sha256.clone(),
            chunk: row.chunk,
            broken: Default::default(),
        })
    }
}

/// Writes a download into its blob already sealed. What's on disk is always ciphertext;
/// `done` (the crash-resume point) is recorded only after the chunks under it are fsynced.
pub(crate) struct EncryptingSink {
    files: Arc<Files>,
    file_id: String,
    id: [u8; 16],
    layout: Layout,
    key: [u8; 32],
    sealer: Sealer,
    blob: tokio::fs::File,
    path: PathBuf,
    /// Plaintext bytes in chunks known durable (as recorded).
    durable: u64,
    sha256: String,
}

impl EncryptingSink {
    /// The row and blob of `file_id`, resuming from the recorded `done`, or new ones. The
    /// row is made only while a cached message lists the file. The blob is opened inside the
    /// cache's own `files/` directory, never creating a directory: after a wipe this fails.
    async fn open(
        files: Arc<Files>,
        file_id: &str,
        size: u64,
        sha256: &str,
    ) -> crate::Result<Self> {
        let id = snapshot::id_bytes(file_id).ok_or_else(|| api_error("file.unknown"))?;
        let chunk = CHUNK;
        // No sweep runs between making the row and opening the blob (see `blob_lock`).
        let holder = files.clone();
        let _blob_guard = holder.blob_lock.lock().await;
        let existing = files.row(file_id).await?;
        let (key, chunk, done) = match existing {
            Some(r) if r.size == size && r.sha256 == sha256 && r.state == "partial" => {
                (r.key, r.chunk, r.done - r.done % r.chunk as u64)
            }
            _ => {
                let key = new_key()?;
                let (fid, sha, k) = (file_id.to_string(), sha256.to_string(), key.to_vec());
                let made = files
                    .cache
                    .db()
                    .call(move |c| {
                        let tx = c.transaction()?;
                        // A journal entry left for this path by an earlier copy is stale now.
                        tx.execute("DELETE FROM deletions WHERE path = 'files/' || ?1", [&fid])?;
                        let made = tx.execute(
                            "INSERT INTO files(file_id, sha256, size, key, chunk, state, done)
                             SELECT ?1, ?2, ?3, ?4, ?5, 'partial', 0
                             WHERE EXISTS (SELECT 1 FROM message_files WHERE file_id = ?1)
                             ON CONFLICT(file_id) DO UPDATE SET sha256 = excluded.sha256,
                                 size = excluded.size, key = excluded.key,
                                 chunk = excluded.chunk, state = 'partial', done = 0",
                            params![fid, sha, size as i64, k, chunk as i64],
                        )?;
                        tx.commit()?;
                        Ok(made)
                    })
                    .await
                    .map_err(store_failed)?;
                if made == 0 {
                    return Err(api_error("file.gone")); // no message lists it any more
                }
                (key, chunk, 0)
            }
        };
        let layout = Layout { chunk, size };
        let path = files.blobs.join(file_id);
        let blob = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .await
            .map_err(|e| io_err(&e))?;
        let index = done / chunk as u64;
        blob.set_len(layout.sealed_offset(index))
            .await
            .map_err(|e| io_err(&e))?;
        let mut sink = Self {
            files,
            file_id: file_id.to_string(),
            id,
            layout,
            key,
            sealer: Sealer::new(&key, id, layout, index),
            blob,
            path,
            durable: done,
            sha256: sha256.to_string(),
        };
        use tokio::io::AsyncSeekExt;
        sink.blob
            .seek(io::SeekFrom::Start(layout.sealed_offset(index)))
            .await
            .map_err(|e| io_err(&e))?;
        Ok(sink)
    }

    async fn record(&mut self) -> io::Result<()> {
        self.blob.sync_data().await?;
        let done = self.sealer.sealed_chunks() * self.layout.chunk as u64;
        let done = done.min(self.layout.size);
        let (id, d) = (self.file_id.clone(), done as i64);
        self.files
            .cache
            .db()
            .call(move |c| {
                c.execute(
                    "UPDATE files SET done = ?2 WHERE file_id = ?1 AND state = 'partial'",
                    params![id, d],
                )
            })
            .await
            .map_err(|_| io::Error::other("store"))?;
        self.durable = done;
        Ok(())
    }
}

#[async_trait::async_trait]
impl DownloadSink for EncryptingSink {
    /// Every byte taken so far, buffered ones included: within this process a resume asks
    /// for exactly what follows them. After a crash, `open` starts at the recorded `done`.
    fn resume_offset(&self) -> u64 {
        (self.sealer.sealed_chunks() * self.layout.chunk as u64 + self.sealer.buffered() as u64)
            .min(self.layout.size)
    }

    /// Start over under a new key (a changed file, a coded body, a range from elsewhere):
    /// the key is replaced in the row before a byte is sealed under it.
    async fn restart(&mut self) -> io::Result<()> {
        let key = new_key().map_err(|_| io::Error::other("randomness"))?;
        let (id, k) = (self.file_id.clone(), key.to_vec());
        let changed = self
            .files
            .cache
            .db()
            .call(move |c| {
                c.execute(
                    "UPDATE files SET key = ?2, done = 0, state = 'partial' WHERE file_id = ?1",
                    params![id, k],
                )
            })
            .await
            .map_err(|_| io::Error::other("store"))?;
        if changed == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the file left the cache",
            ));
        }
        self.blob.set_len(0).await?;
        use tokio::io::AsyncSeekExt;
        self.blob.seek(io::SeekFrom::Start(0)).await?;
        self.key = key;
        self.sealer = Sealer::new(&key, self.id, self.layout, 0);
        self.durable = 0;
        Ok(())
    }

    async fn write_chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
        let before = self.sealer.sealed_chunks();
        let sealed = self
            .sealer
            .push(bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "longer than the file"))?;
        if !sealed.is_empty() {
            self.blob.write_all(&sealed).await?;
        }
        let durable_chunks = self.durable / self.layout.chunk as u64;
        if self.sealer.sealed_chunks() > before
            && self.sealer.sealed_chunks() - durable_chunks >= SYNC_EVERY
        {
            self.record().await?;
        }
        Ok(())
    }

    async fn finish(&mut self, sha256: &str) -> Result<(), SinkError> {
        if !self.sealer.is_complete() || sha256 != self.sha256 {
            return Err(SinkError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the download ended early",
            )));
        }
        self.blob.flush().await.map_err(SinkError::Io)?;
        self.blob.sync_all().await.map_err(SinkError::Io)?;
        if let Some(dir) = self.path.parent() {
            if let Ok(d) = tokio::fs::File::open(dir).await {
                let _ = d.sync_all().await;
            }
        }
        let (path, key, id, size, sha, chunk) = (
            self.path.clone(),
            self.key,
            self.id,
            self.layout.size,
            self.sha256.clone(),
            self.layout.chunk,
        );
        let checked = tokio::task::spawn_blocking(move || {
            snapshot::verify(&path, &key, id, size, &sha, chunk)
        })
        .await
        .map_err(|_| SinkError::Io(io::Error::other("verify")))?;
        if checked.is_err() {
            let _ = self.restart().await;
            return Err(SinkError::Mismatch);
        }
        let fid = self.file_id.clone();
        let marked = self
            .files
            .cache
            .db()
            .call(move |c| {
                c.execute(
                    "UPDATE files SET state = 'complete', done = size,
                         last_used = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                     WHERE file_id = ?1",
                    [&fid],
                )
            })
            .await
            .map_err(|_| SinkError::Io(io::Error::other("store")))?;
        if marked == 0 {
            // Dropped while downloading (its message went): nothing to keep.
            return Err(SinkError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "gone",
            )));
        }
        self.files
            .cache
            .announce(CacheEvent::Files(vec![self.file_id.clone()]));
        Ok(())
    }

    /// Keep the partial: fsync it and record how far it's durable.
    async fn abort(&mut self) {
        let _ = self.record().await;
    }
}

// ---- Open: the directory, the sniff, the copy ----

/// This store's Open directory: private, per user and per store, never shared.
pub(crate) fn open_dir_for(store_id: &str) -> Option<PathBuf> {
    if store_id.is_empty() {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        Some(std::env::temp_dir().join("brook").join(store_id))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
        let base = match std::env::var("FLATPAK_ID") {
            // Inside a Flatpak, the part of the runtime dir the host shares with the app.
            Ok(app) if !app.is_empty() => runtime.join("app").join(app),
            _ => runtime,
        };
        Some(base.join("brook").join(store_id))
    }
}

/// How much of a file Open sniffs.
const SNIFF: usize = 4096;

/// Kinds of file Open hands to the system, each with the extensions it may carry.
/// Anything else is Save only: the system picks the app by extension and type, so a
/// denylist of formats can't cover what might run (a `.jar` is a zip, like `.docx`; an
/// `.msi` is OLE, like `.doc`; `.html` and `.svg` run script in a browser; `.deb`,
/// `.rpm` and `.flatpakref` open installers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pdf,
    Png,
    Jpeg,
    Gif,
    Webp,
    /// Plain text (read as text by an editor).
    Text,
    /// Office Open XML and OpenDocument: zip containers, allowed only under their own
    /// extensions (the macro-enabled `m` variants are not on the list).
    OfficeZip,
    Mp3,
    Mp4,
    Ogg,
    Wav,
    Flac,
    Matroska,
}

fn kind_for_extension(ext: &str) -> Option<Kind> {
    Some(match ext {
        "pdf" => Kind::Pdf,
        "png" => Kind::Png,
        "jpg" | "jpeg" => Kind::Jpeg,
        "gif" => Kind::Gif,
        "webp" => Kind::Webp,
        "txt" | "md" | "log" | "csv" => Kind::Text,
        "docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp" => Kind::OfficeZip,
        "mp3" => Kind::Mp3,
        "mp4" | "m4a" | "mov" => Kind::Mp4,
        "ogg" | "oga" | "opus" => Kind::Ogg,
        "wav" => Kind::Wav,
        "flac" => Kind::Flac,
        "mkv" | "webm" => Kind::Matroska,
        _ => return None,
    })
}

/// Whether the first bytes are that kind.
fn looks_like(kind: Kind, head: &[u8]) -> bool {
    match kind {
        Kind::Pdf => head.starts_with(b"%PDF-"),
        Kind::Png => head.starts_with(b"\x89PNG\r\n\x1a\n"),
        Kind::Jpeg => head.starts_with(b"\xff\xd8\xff"),
        Kind::Gif => head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a"),
        Kind::Webp => head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP",
        Kind::Text => is_plain_text(head),
        Kind::OfficeZip => head.starts_with(b"PK\x03\x04"),
        Kind::Mp3 => {
            head.starts_with(b"ID3")
                || (head.len() >= 2 && head[0] == 0xff && head[1] & 0xe0 == 0xe0)
        }
        Kind::Mp4 => head.len() >= 8 && &head[4..8] == b"ftyp",
        Kind::Ogg => head.starts_with(b"OggS"),
        Kind::Wav => head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WAVE",
        Kind::Flac => head.starts_with(b"fLaC"),
        Kind::Matroska => head.starts_with(b"\x1a\x45\xdf\xa3"),
    }
}

/// Text an editor shows as text: UTF-8, no NULs, and not markup a viewer might render
/// (HTML, SVG, XML) whatever the extension says.
fn is_plain_text(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    // A cut at the sniff limit may split a character: judge only what's complete.
    let text = match std::str::from_utf8(head) {
        Ok(t) => t,
        Err(e) if e.error_len().is_none() => {
            std::str::from_utf8(&head[..e.valid_up_to()]).unwrap_or("")
        }
        Err(_) => return false,
    };
    // GIO prefers a sniffed subtype of the name's type (text/html and SVG under
    // text/plain), and its rules match tags anywhere in the first bytes: refuse any
    // tag-like `<` at all, not only at the start.
    !text
        .as_bytes()
        .windows(2)
        .any(|w| w[0] == b'<' && (w[1].is_ascii_alphabetic() || matches!(w[1], b'!' | b'?' | b'/')))
}

/// Open may hand this file to the system: its extension is on the list, its bytes are
/// that kind, and they aren't an executable or a launcher anyway (a second check).
pub(crate) fn openable(filename: &str, head: &[u8]) -> bool {
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    kind_for_extension(&ext).is_some_and(|kind| looks_like(kind, head)) && !is_launchable(head)
}

/// Executables and launchers, sniffed from the first bytes: those are Save only.
pub(crate) fn is_launchable(head: &[u8]) -> bool {
    const MAGICS: &[&[u8]] = &[
        b"\x7fELF",
        b"MZ",
        b"#!",
        b"\xfe\xed\xfa\xce",
        b"\xfe\xed\xfa\xcf",
        b"\xce\xfa\xed\xfe",
        b"\xcf\xfa\xed\xfe",
        b"\xca\xfe\xba\xbe",
        b"\xbe\xba\xfe\xca",
    ];
    if MAGICS.iter().any(|m| head.starts_with(m)) {
        return true;
    }
    // A desktop entry, after an optional BOM and whitespace.
    let text = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    let start = text
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(text.len());
    text[start..].starts_with(b"[Desktop Entry]")
}

/// The server's sanitised name, and only its last component.
fn safe_leaf(filename: &str) -> String {
    let leaf = filename.rsplit(['/', '\\']).next().unwrap_or("");
    if leaf.is_empty() || leaf == "." || leaf == ".." {
        "file".into()
    } else {
        leaf.to_string()
    }
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

async fn private_file(path: &Path) -> io::Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path).await
}

/// On the Mac, tag an Open copy as downloaded, so Gatekeeper treats it with care.
fn mark_downloaded(_path: &Path) {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let name = c"com.apple.quarantine";
        let value = b"0081;00000000;Brook;";
        if let Ok(path) = std::ffi::CString::new(_path.as_os_str().as_bytes()) {
            // SAFETY: valid C strings and a buffer of the given length.
            unsafe {
                libc::setxattr(
                    path.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                    0,
                );
            }
        }
    }
}

fn clear_dir(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
        }
    }
}

async fn source_reader(
    source: &SnapshotSource,
) -> crate::Result<Box<dyn tokio::io::AsyncRead + Send + Unpin>> {
    use crate::transfer::UploadSource;
    source.reader().await.map_err(|e| io_err(&e))
}

async fn read_up_to(
    reader: &mut (dyn tokio::io::AsyncRead + Send + Unpin),
    buf: &mut [u8],
) -> crate::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = reader.read(&mut buf[n..]).await.map_err(|e| io_err(&e))?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

fn new_key() -> crate::Result<[u8; 32]> {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).map_err(|_| api_error("local.store"))?;
    Ok(key)
}

fn random_hex() -> String {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn io_err(e: &io::Error) -> Error {
    Error::Api {
        code: "transfer.io".into(),
        message: format!("{:?}", e.kind()),
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
