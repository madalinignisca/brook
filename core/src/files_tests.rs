//! The file cache (keep-offline plan, core PR 1 tests): downloads sealed as they arrive and
//! resumed from what's durable, one download per file, the store's own session only, Open's
//! refusals and its private copies, eviction, close and wipe, and reconciliation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{watch, Notify};

use crate::apply::{apply, Batch, MemberRow, MessageRow, Row};
use crate::cache::{Cache, CacheEvent, History};
use crate::files::{is_launchable, Download, FileCacheState, Files};
use crate::store::{self, Kind, Opened};
use crate::sync::{Fetch, Page};
use crate::transfer::{DownloadSink, Flags, TransferId, TransferState, Transfers};
use crate::{Error, InMemoryKeySlot, KeySlot, KeyStore};

const MIB: usize = 1 << 20;
const F1: &str = "0190a000-0000-7000-8000-0000000000f1";
const F2: &str = "0190a000-0000-7000-8000-0000000000f2";

fn sha(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// A server for the file cache: serves each file from whatever the sink already holds, and
/// can fail part way, answer 404, or hold until released (checking the flags as it waits).
#[derive(Default)]
struct Server {
    files: Mutex<HashMap<String, Vec<u8>>>,
    fail_after: Mutex<Option<u64>>,
    gone: Mutex<bool>,
    hold: Mutex<Option<(u64, Arc<Notify>)>>,
    /// `(file id, resume offset, epoch)` per request.
    asked: Mutex<Vec<(String, u64, u64)>>,
}

impl Server {
    fn put(&self, id: &str, content: Vec<u8>) {
        self.files.lock().unwrap().insert(id.into(), content);
    }
    fn asked(&self) -> Vec<(String, u64, u64)> {
        self.asked.lock().unwrap().clone()
    }
}

fn stopped(flags: &Flags) -> Option<Error> {
    let code = if flags.cancel.load(Ordering::SeqCst) {
        "transfer.cancelled"
    } else if flags.pause.load(Ordering::SeqCst) {
        "transfer.paused"
    } else {
        return None;
    };
    Some(Error::Api {
        code: code.into(),
        message: String::new(),
    })
}

#[async_trait::async_trait]
impl Download for Server {
    async fn download(
        &self,
        _: TransferId,
        flags: &Arc<Flags>,
        file_id: &str,
        sha256: &str,
        _size: u64,
        sink: &mut dyn DownloadSink,
        epoch: u64,
    ) -> crate::Result<()> {
        let offset = sink.resume_offset();
        self.asked
            .lock()
            .unwrap()
            .push((file_id.into(), offset, epoch));
        if *self.gone.lock().unwrap() {
            return Err(crate::transfer::gone_error());
        }
        let content = self.files.lock().unwrap().get(file_id).cloned().unwrap();
        let fail_after = self.fail_after.lock().unwrap().take();
        let mut hold = self.hold.lock().unwrap().clone();
        let mut at = offset as usize;
        while at < content.len() {
            if let Some((when, release)) = &hold {
                if at as u64 >= *when {
                    loop {
                        if let Some(stop) = stopped(flags) {
                            return Err(stop);
                        }
                        let released =
                            tokio::time::timeout(Duration::from_millis(10), release.notified())
                                .await;
                        if released.is_ok() {
                            break;
                        }
                    }
                    hold = None; // held once
                }
            }
            if let Some(stop) = stopped(flags) {
                return Err(stop);
            }
            if fail_after.is_some_and(|f| at as u64 >= f) {
                return Err(Error::Api {
                    code: "transfer.network".into(),
                    message: String::new(),
                });
            }
            let end = (at + 64 * 1024).min(content.len());
            sink.write_chunk(&content[at..end])
                .await
                .map_err(|e| Error::Api {
                    code: "transfer.io".into(),
                    message: format!("{e}"),
                })?;
            at = end;
        }
        sink.finish(sha256).await.map_err(|_| Error::Api {
            code: "transfer.integrity".into(),
            message: String::new(),
        })
    }
}

struct Nothing;

#[async_trait::async_trait]
impl Fetch for Nothing {
    async fn page(&self, _: &str) -> Result<Page, Error> {
        Err(Error::Timeout)
    }
}

#[async_trait::async_trait]
impl History for Nothing {
    async fn page(&self, _: &str, _: Option<&str>, _: usize) -> Result<Vec<Value>, Error> {
        Ok(vec![])
    }
}

struct Setup {
    cache: Arc<Cache>,
    files: Arc<Files>,
    server: Arc<Server>,
    transfers: Arc<Transfers>,
    session: watch::Sender<Option<u64>>,
    store: PathBuf,
    open_dir: PathBuf,
    _root: tempfile::TempDir,
}

fn attachment(id: &str, content: &[u8], name: &str) -> Value {
    json!({
        "id": id, "channel_id": "c", "uploader_id": "u", "filename": name,
        "original_name": name, "size": content.len(), "content_type": "application/pdf",
        "status": "committed", "sha256": sha(content), "created_at": "2026-09-26T00:00:00Z"
    })
}

fn message(id: &str, seq: i64, files: Vec<Value>) -> Value {
    json!({ "id": id, "channel_id": "c", "author_id": "u", "body": "", "seq": seq,
            "created_at": "2026-09-26T00:00:00Z", "attachments": files })
}

/// A cache holding channel `c`, where `m1` carries `F1` (and `F2` when given).
async fn setup_with(contents: &[(&str, Vec<u8>, &str)]) -> Setup {
    let root = tempfile::tempdir().unwrap();
    let store = root.path().join("store");
    std::fs::create_dir(&store).unwrap();
    let slot: Arc<dyn KeySlot> = Arc::new(InMemoryKeySlot::default());
    let db = match store::open(&store, Kind::Cache, "s", &KeyStore::new(slot)).unwrap() {
        Opened::Ready { db, .. } => db,
        other => panic!("{other:?}"),
    };
    let cache = Cache::new(db, "me".into(), Arc::new(Nothing), Arc::new(Nothing));
    let server = Arc::new(Server::default());
    let files_json: Vec<Value> = contents
        .iter()
        .map(|(id, c, name)| {
            server.put(id, c.clone());
            attachment(id, c, name)
        })
        .collect();
    let m1 = message("m1", 10, files_json);
    cache
        .db()
        .call(move |c| {
            let tx = c.transaction()?;
            apply(
                &tx,
                "me",
                &Batch {
                    channels: vec![Row {
                        id: "c".into(),
                        seq: 10,
                        json: json!({ "id": "c", "seq": 10 }),
                    }],
                    memberships: vec![MemberRow {
                        channel_id: "c".into(),
                        user_id: "me".into(),
                        seq: 10,
                        json: json!({ "channel_id": "c", "user_id": "me", "seq": 10 }),
                    }],
                    messages: vec![MessageRow {
                        id: "m1".into(),
                        channel_id: "c".into(),
                        seq: 10,
                        created_at: "2026-09-26T00:00:00Z".into(),
                        json: m1,
                    }],
                    ..Batch::default()
                },
            )?;
            tx.commit()
        })
        .await
        .unwrap();
    let open_dir = root.path().join("run").join("brook").join("s");
    let transfers = Arc::new(Transfers::new());
    let (session, rx) = watch::channel(Some(1));
    let files = Files::open(
        cache.clone(),
        &store,
        Some(open_dir.clone()),
        server.clone(),
        transfers.clone(),
        rx,
    )
    .await;
    Setup {
        cache,
        files,
        server,
        transfers,
        session,
        store,
        open_dir,
        _root: root,
    }
}

impl Setup {
    async fn reopen(&mut self) {
        self.files.close().await;
        self.files = Files::open(
            self.cache.clone(),
            &self.store,
            Some(self.open_dir.clone()),
            self.server.clone(),
            self.transfers.clone(),
            self.session.subscribe(),
        )
        .await;
    }

    fn blob(&self, id: &str) -> PathBuf {
        self.store.join("files").join(id)
    }
}

async fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..500 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn a_cached_file_is_sealed_on_disk_and_saves_offline() {
    let content = bytes(2 * MIB + 77, 1);
    let s = setup_with(&[(F1, content.clone(), "report.pdf")]).await;
    s.files.cache_file(TransferId::new(), F1).await.unwrap();
    assert_eq!(s.files.state(F1).await.unwrap(), FileCacheState::Cached);
    let on_disk = std::fs::read(s.blob(F1)).unwrap();
    assert!(
        !on_disk.windows(32).any(|w| w == &content[1000..1032]),
        "plaintext on disk"
    );
    // The server is gone: Save still works, from the cache.
    *s.server.gone.lock().unwrap() = true;
    let dest = s._root.path().join("saved.pdf");
    assert_eq!(s.files.save_from_cache(F1, &dest).await.unwrap(), Some(()));
    assert_eq!(std::fs::read(&dest).unwrap(), content);
    assert_eq!(
        s.server.asked().len(),
        1,
        "a cached file is never fetched again"
    );
}

#[tokio::test]
async fn a_broken_download_resumes_from_what_was_recorded() {
    let content = bytes(10 * MIB + 5, 2);
    let s = setup_with(&[(F1, content.clone(), "big.bin")]).await;
    *s.server.fail_after.lock().unwrap() = Some((9 * MIB + MIB / 2) as u64);
    let err = s.files.cache_file(TransferId::new(), F1).await.unwrap_err();
    assert!(
        matches!(&err, Error::Api { code, .. } if code == "transfer.network"),
        "{err:?}"
    );
    // Nine whole chunks were sealed and recorded; the half chunk after them wasn't.
    assert_eq!(
        s.files.state(F1).await.unwrap(),
        FileCacheState::Partial {
            done: 9 * MIB as u64,
            size: content.len() as u64
        }
    );
    s.files.cache_file(TransferId::new(), F1).await.unwrap();
    let asked: Vec<u64> = s.server.asked().iter().map(|a| a.1).collect();
    assert_eq!(asked, [0, 9 * MIB as u64]);
    let dest = s._root.path().join("big.out");
    s.files.save_from_cache(F1, &dest).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), content);
}

#[tokio::test]
async fn a_partial_survives_a_restart_of_the_cache() {
    let content = bytes(3 * MIB + 9, 3);
    let mut s = setup_with(&[(F1, content.clone(), "a.bin")]).await;
    *s.server.fail_after.lock().unwrap() = Some((2 * MIB + 10) as u64);
    let _ = s.files.cache_file(TransferId::new(), F1).await;
    s.reopen().await;
    s.files.cache_file(TransferId::new(), F1).await.unwrap();
    assert_eq!(s.server.asked()[1].1, 2 * MIB as u64);
    assert_eq!(s.files.state(F1).await.unwrap(), FileCacheState::Cached);
}

#[tokio::test]
async fn an_unknown_file_is_refused() {
    let s = setup_with(&[(F1, bytes(10, 4), "a.bin")]).await;
    let err = s.files.cache_file(TransferId::new(), F2).await.unwrap_err();
    assert!(
        matches!(&err, Error::Api { code, .. } if code == "file.unknown"),
        "{err:?}"
    );
    assert!(s.server.asked().is_empty());
}

#[tokio::test]
async fn a_deleted_file_is_gone_and_leaves_the_cache() {
    let s = setup_with(&[(F1, bytes(3 * MIB, 5), "a.bin")]).await;
    *s.server.fail_after.lock().unwrap() = Some(MIB as u64 + 1);
    let _ = s.files.cache_file(TransferId::new(), F1).await; // a partial
    assert!(s.blob(F1).exists());
    *s.server.gone.lock().unwrap() = true;
    let mut events = s.cache.events();
    let err = s.files.cache_file(TransferId::new(), F1).await.unwrap_err();
    assert!(
        matches!(&err, Error::Api { code, .. } if code == "file.gone"),
        "{err:?}"
    );
    assert_eq!(s.files.state(F1).await.unwrap(), FileCacheState::NotCached);
    assert!(!s.blob(F1).exists());
    assert_eq!(
        events.recv().await.unwrap(),
        CacheEvent::Files(vec![F1.into()])
    );
    // No message lists it any more.
    let again = s.files.cache_file(TransferId::new(), F1).await.unwrap_err();
    assert!(matches!(&again, Error::Api { code, .. } if code == "file.unknown"));
}

#[tokio::test]
async fn a_file_deleted_from_its_message_is_unlinked() {
    let s = setup_with(&[(F1, bytes(100, 6), "a.bin"), (F2, bytes(200, 7), "b.bin")]).await;
    s.files.cache_file(TransferId::new(), F1).await.unwrap();
    s.files.cache_file(TransferId::new(), F2).await.unwrap();
    // The server restamps m1 with the shorter list.
    let shorter = message("m1", 20, vec![attachment(F1, &bytes(100, 6), "a.bin")]);
    s.cache.live_event("message.update", &shorter).await;
    wait_until("the blob unlinked", || !s.blob(F2).exists()).await;
    assert!(s.blob(F1).exists());
    // A live delete of the message takes the rest.
    s.cache
        .live_event(
            "message.delete",
            &json!({ "id": "m1", "channel_id": "c", "seq": 30 }),
        )
        .await;
    wait_until("the last blob unlinked", || !s.blob(F1).exists()).await;
}

#[tokio::test]
async fn a_second_caller_joins_and_cancelling_one_keeps_the_other() {
    let content = bytes(2 * MIB, 8);
    let s = setup_with(&[(F1, content.clone(), "a.bin")]).await;
    let release = Arc::new(Notify::new());
    *s.server.hold.lock().unwrap() = Some((MIB as u64, release.clone()));
    let (a, b) = (TransferId::new(), TransferId::new());
    let first = tokio::spawn({
        let f = s.files.clone();
        async move { f.cache_file(a, F1).await }
    });
    wait_until("the download held", || s.server.asked().len() == 1).await;
    let second = tokio::spawn({
        let f = s.files.clone();
        async move { f.cache_file(b, F1).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    s.transfers.flag(a).cancel.store(true, Ordering::SeqCst); // the first caller cancels
    let first = first.await.unwrap().unwrap_err();
    assert!(matches!(&first, Error::Api { code, .. } if code == "transfer.cancelled"));
    release.notify_one();
    second.await.unwrap().unwrap();
    assert_eq!(s.server.asked().len(), 1, "one download for both");
    assert_eq!(s.files.state(F1).await.unwrap(), FileCacheState::Cached);
}

#[tokio::test]
async fn when_every_caller_cancels_the_download_stops_and_keeps_its_partial() {
    let s = setup_with(&[(F1, bytes(3 * MIB, 9), "a.bin")]).await;
    let release = Arc::new(Notify::new());
    *s.server.hold.lock().unwrap() = Some(((2 * MIB) as u64, release));
    let a = TransferId::new();
    let task = tokio::spawn({
        let f = s.files.clone();
        async move { f.cache_file(a, F1).await }
    });
    wait_until("the download held", || s.server.asked().len() == 1).await;
    s.transfers.flag(a).cancel.store(true, Ordering::SeqCst);
    assert!(task.await.unwrap().is_err());
    for _ in 0..500 {
        if s.files.state(F1).await.unwrap()
            == (FileCacheState::Partial {
                done: 2 * MIB as u64,
                size: 3 * MIB as u64,
            })
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the partial wasn't recorded: {:?}", s.files.state(F1).await);
}

#[tokio::test]
async fn a_session_change_pauses_the_download_under_its_own_epoch_only() {
    let s = setup_with(&[(F1, bytes(2 * MIB, 10), "a.bin")]).await;
    let release = Arc::new(Notify::new());
    *s.server.hold.lock().unwrap() = Some((MIB as u64, release));
    let task = tokio::spawn({
        let f = s.files.clone();
        async move { f.cache_file(TransferId::new(), F1).await }
    });
    wait_until("the download held", || s.server.asked().len() == 1).await;
    s.session.send_replace(None); // signed out (or another user)
    let err = task.await.unwrap().unwrap_err();
    assert!(
        matches!(&err, Error::Api { code, .. } if code == "transfer.paused"),
        "{err:?}"
    );
    assert!(
        s.server.asked().iter().all(|a| a.2 == 1),
        "only the store's epoch"
    );
    // Signed out: nothing starts.
    let err = s.files.cache_file(TransferId::new(), F1).await.unwrap_err();
    assert!(matches!(err, Error::NotAuthenticated), "{err:?}");
}

#[tokio::test]
async fn close_joins_a_download_and_a_wipe_after_it_leaves_no_store() {
    let s = setup_with(&[(F1, bytes(3 * MIB, 11), "a.bin")]).await;
    let release = Arc::new(Notify::new());
    *s.server.hold.lock().unwrap() = Some((MIB as u64, release.clone()));
    let task = tokio::spawn({
        let f = s.files.clone();
        async move { f.cache_file(TransferId::new(), F1).await }
    });
    wait_until("the download held", || s.server.asked().len() == 1).await;
    s.files.close().await; // cancels and joins
    s.cache.clone().close().await;
    std::fs::remove_dir_all(&s.store).unwrap(); // the wipe
    release.notify_waiters();
    assert!(task.await.unwrap().is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!s.store.exists(), "a late write recreated the wiped store");
    let err = s.files.cache_file(TransferId::new(), F1).await.unwrap_err();
    assert!(matches!(&err, Error::Api { code, .. } if code == "local.unavailable"));
}

#[tokio::test]
async fn open_refuses_launchers_and_makes_a_private_copy() {
    let pdf = [b"%PDF-1.7\n".as_slice(), &bytes(5000, 12)].concat();
    let elf = [b"\x7fELF".as_slice(), &bytes(100, 13)].concat();
    let s = setup_with(&[(F1, pdf.clone(), "../../report.pdf"), (F2, elf, "tool")]).await;
    let err = s.files.open_file(TransferId::new(), F2).await.unwrap_err();
    assert!(
        matches!(&err, Error::Api { code, .. } if code == "file.open_refused"),
        "{err:?}"
    );
    let path = s.files.open_file(TransferId::new(), F1).await.unwrap();
    assert!(path.starts_with(&s.open_dir), "{path:?}");
    assert_eq!(
        path.file_name().unwrap(),
        "report.pdf",
        "only the last component"
    );
    assert_eq!(std::fs::read(&path).unwrap(), pdf);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }
    s.files.clear_open_copies();
    assert!(!path.exists());
}

#[test]
fn launchers_are_sniffed_from_their_bytes() {
    for head in [
        b"\x7fELF\x02\x01".as_slice(),
        b"MZ\x90\x00",
        b"#!/bin/sh\n",
        b"\xcf\xfa\xed\xfe",
        b"\xca\xfe\xba\xbe",
        b"[Desktop Entry]\nExec=x",
        b"\xef\xbb\xbf  \n[Desktop Entry]",
    ] {
        assert!(is_launchable(head), "{head:?}");
    }
    for head in [
        b"%PDF-1.7".as_slice(),
        b"\x89PNG",
        b"hello",
        b"",
        b"# notes",
    ] {
        assert!(!is_launchable(head), "{head:?}");
    }
}

#[tokio::test]
async fn eviction_drops_the_least_recently_used_but_never_the_one_just_cached() {
    let (a, b) = (bytes(MIB, 14), bytes(MIB, 15));
    let s = setup_with(&[(F1, a, "a.bin"), (F2, b, "b.bin")]).await;
    s.files.set_cap(MIB as u64 + 10);
    s.files.cache_file(TransferId::new(), F1).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    s.files.cache_file(TransferId::new(), F2).await.unwrap();
    assert_eq!(s.files.state(F1).await.unwrap(), FileCacheState::NotCached);
    assert!(!s.blob(F1).exists());
    assert_eq!(s.files.state(F2).await.unwrap(), FileCacheState::Cached);
    // Evicted is not gone: it downloads again.
    s.files.cache_file(TransferId::new(), F1).await.unwrap();
    assert_eq!(s.files.state(F1).await.unwrap(), FileCacheState::Cached);
}

#[tokio::test]
async fn reconciliation_removes_orphans_short_partials_and_open_copies() {
    let mut s = setup_with(&[(F1, bytes(3 * MIB, 16), "a.bin")]).await;
    *s.server.fail_after.lock().unwrap() = Some(2 * MIB as u64 + 1);
    let _ = s.files.cache_file(TransferId::new(), F1).await; // a partial at 2 MiB
    std::fs::write(s.store.join("files").join("stray"), b"x").unwrap();
    std::fs::create_dir_all(s.open_dir.join("old")).unwrap();
    std::fs::write(s.open_dir.join("old").join("copy.pdf"), b"plain").unwrap();
    // The partial's blob lost its tail (it claims more than is there).
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(s.blob(F1))
        .unwrap();
    f.set_len(10).unwrap();
    drop(f);
    s.reopen().await;
    assert!(!s.store.join("files").join("stray").exists());
    assert!(!s.open_dir.join("old").exists());
    assert_eq!(s.files.state(F1).await.unwrap(), FileCacheState::NotCached);
    assert!(!s.blob(F1).exists());
}

#[tokio::test]
async fn progress_arrives_under_the_callers_id() {
    let s = setup_with(&[(F1, bytes(MIB, 17), "a.bin")]).await;
    let id = TransferId::new();
    let mut events = s.transfers.subscribe();
    s.files.cache_file(id, F1).await.unwrap();
    let mut last = None;
    while let Ok(e) = events.try_recv() {
        if e.id == id {
            last = Some(e.state);
        }
    }
    assert_eq!(last, Some(TransferState::Done));
}
