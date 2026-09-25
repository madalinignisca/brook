//! Encrypted stores (plan C1): at rest only ciphertext, the key check decides missing vs
//! damaged vs locked, one opener per store, and a reset that needs no read.

use std::path::Path;
use std::sync::Arc;

use crate::store::{self, Kind, Opened, Rebuilt, StoreError};
use crate::{InMemoryKeySlot, KeySlot, KeySlotError, KeyStore};

const SENTINEL: &str = "BROOK-SENTINEL-c1-5e7a";

fn keys(slot: &Arc<InMemoryKeySlot>) -> KeyStore<dyn KeySlot> {
    KeyStore::new(slot.clone() as Arc<dyn KeySlot>)
}

fn ready(o: Result<Opened, StoreError>) -> (store::Db, Option<Rebuilt>) {
    match o.unwrap() {
        Opened::Ready { db, rebuilt } => (db, rebuilt),
        other => panic!("not ready: {other:?}"),
    }
}

/// Every file under `dir`, including SQLite's `-wal` and `-shm`, scanned for `needle`.
fn plaintext_anywhere(dir: &Path, needle: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if let Ok(bytes) = std::fs::read(&p) {
            if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                hits.push(p.file_name().unwrap().to_string_lossy().to_string());
            }
        }
    }
    hits
}

async fn write_sentinel(db: &store::Db, n: usize) {
    db.call(move |c| {
        for i in 0..n {
            c.execute(
                "INSERT INTO users(id, seq, json) VALUES (?1, 1, ?2)",
                [format!("u{i}"), format!("{{\"handle\":\"{SENTINEL}\"}}")],
            )?;
        }
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn nothing_under_the_store_is_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (db, rebuilt) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    assert_eq!(rebuilt, None);
    write_sentinel(&db, 200).await;
    assert!(dir.path().join("cache.db-wal").exists(), "WAL mode");
    assert_eq!(
        plaintext_anywhere(dir.path(), SENTINEL),
        Vec::<String>::new(),
        "after writes"
    );
    // Mid-transaction: the pages spill to the WAL before the commit.
    db.call(|c| {
        c.execute_batch("BEGIN;")?;
        for i in 0..500 {
            c.execute(
                "INSERT INTO users(id, seq, json) VALUES (?1, 1, ?2)",
                [format!("t{i}"), format!("{SENTINEL}-tx")],
            )?;
        }
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        plaintext_anywhere(dir.path(), SENTINEL),
        Vec::<String>::new(),
        "mid-transaction"
    );
    // A real crash is `a_crash_mid_transaction_leaves_only_ciphertext_and_committed_rows`.
    db.close().await;
}

#[tokio::test]
async fn a_store_opens_only_with_its_own_key() {
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (db, _) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    write_sentinel(&db, 1).await;
    db.close().await;
    // Straight SQLite with another key: refused.
    let conn = rusqlite::Connection::open(dir.path().join("cache.db")).unwrap();
    conn.execute_batch(&format!("PRAGMA key = \"x'{}'\";", "ab".repeat(32)))
        .unwrap();
    assert!(conn
        .query_row("SELECT count(*) FROM users", [], |r| r.get::<_, i64>(0))
        .is_err());
}

/// Crypto-erase: once the slot is destroyed, a copy of the database taken before is useless.
#[tokio::test]
async fn a_copied_store_is_unreadable_once_its_slot_is_destroyed() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (db, _) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    write_sentinel(&db, 1).await;
    db.close().await;
    for f in ["cache.db", "cache.check"] {
        std::fs::copy(dir.path().join(f), elsewhere.path().join(f)).unwrap();
    }
    assert!(store::reset(dir.path(), Kind::Cache, "s1", &keys(&slot)).unwrap());
    assert!(!slot.contains("cache:s1"));
    // The copy, opened under the same slot name: a new key, which isn't the copy's.
    let (db, rebuilt) = ready(store::open(
        elsewhere.path(),
        Kind::Cache,
        "s1",
        &keys(&slot),
    ));
    assert_eq!(rebuilt, Some(Rebuilt::KeyMissing));
    let users: i64 = db
        .call(|c| c.query_row("SELECT count(*) FROM users", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(users, 0, "the old contents came back");
}

#[tokio::test]
async fn an_unreadable_key_deletes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (db, _) = ready(store::open(dir.path(), Kind::Outbox, "s1", &keys(&slot)));
    db.close().await;
    let before = std::fs::read(dir.path().join("outbox.db")).unwrap();
    for err in [KeySlotError::Unavailable, KeySlotError::Fatal(-34018)] {
        slot.fail_next("load", err);
        let o = store::open(dir.path(), Kind::Outbox, "s1", &keys(&slot)).unwrap();
        assert!(matches!(o, Opened::Locked), "{o:?}");
        assert_eq!(std::fs::read(dir.path().join("outbox.db")).unwrap(), before);
        assert!(dir.path().join("outbox.check").exists());
    }
}

/// The key was lost (or a crash split a new key from the rebuild): the check doesn't match,
/// so the old database is remade, and the caller hears it (an outbox says what was lost).
#[tokio::test]
async fn a_missing_key_rebuilds_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (db, _) = ready(store::open(dir.path(), Kind::Outbox, "s1", &keys(&slot)));
    db.call(|c| {
        c.execute(
            "INSERT INTO outbox(client_id, channel_id, body, state, created_at) VALUES ('c', 'ch', 'hi', 'pending', 't')",
            [],
        )
    })
    .await
    .unwrap();
    db.close().await;
    slot.put("outbox:s1", vec![7; 32]); // not this database's key
    let (db, rebuilt) = ready(store::open(dir.path(), Kind::Outbox, "s1", &keys(&slot)));
    assert_eq!(rebuilt, Some(Rebuilt::KeyMissing));
    let rows: i64 = db
        .call(|c| c.query_row("SELECT count(*) FROM outbox", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

/// The key is right (the check matches) but the database is damaged: nothing is deleted.
#[tokio::test]
async fn a_damaged_store_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (db, _) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    write_sentinel(&db, 50).await;
    db.close().await;
    let path = dir.path().join("cache.db");
    let mut bytes = std::fs::read(&path).unwrap();
    for b in bytes.iter_mut().take(4096) {
        *b ^= 0x5a; // page 1 garbled
    }
    std::fs::write(&path, &bytes).unwrap();
    let o = store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)).unwrap();
    assert!(matches!(o, Opened::Damaged), "{o:?}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "a damaged store was touched"
    );
    // The way out needs no read: the key goes, then the files.
    assert!(store::reset(dir.path(), Kind::Cache, "s1", &keys(&slot)).unwrap());
    assert!(!path.exists());
    assert!(!slot.contains("cache:s1"));
}

#[tokio::test]
async fn one_opener_per_store() {
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (db, _) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    assert_eq!(
        store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)).unwrap_err(),
        StoreError::AlreadyOpen
    );
    assert_eq!(
        store::reset(dir.path(), Kind::Cache, "s1", &keys(&slot)).unwrap_err(),
        StoreError::AlreadyOpen,
        "a reset under an open store"
    );
    db.close().await; // waits for the thread: the store is free the moment this returns
    let (_db, rebuilt) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    assert_eq!(
        rebuilt, None,
        "a reopen with the same key must keep the data"
    );
}

#[tokio::test]
async fn a_cache_in_another_format_rebuilds_but_an_outbox_waits() {
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    for kind in [Kind::Cache, Kind::Outbox] {
        let (db, _) = ready(store::open(dir.path(), kind, "s1", &keys(&slot)));
        db.call(|c| c.execute("UPDATE meta SET format = 0", []))
            .await
            .unwrap();
        db.close().await;
    }
    let (_cache, rebuilt) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    assert_eq!(rebuilt, Some(Rebuilt::FormatChanged));
    let o = store::open(dir.path(), Kind::Outbox, "s1", &keys(&slot)).unwrap();
    assert!(matches!(o, Opened::NeedsRebuild), "{o:?}");
    assert!(
        dir.path().join("outbox.db").exists(),
        "the outbox was rebuilt before surfacing"
    );
    let (_outbox, rebuilt) = ready(store::rebuild(dir.path(), Kind::Outbox, "s1", &keys(&slot)));
    assert_eq!(rebuilt, None);
}

#[tokio::test]
async fn store_ids_are_random_and_stable() {
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (index, _) = ready(store::open(dir.path(), Kind::Index, "", &keys(&slot)));
    let a = store::store_id(&index, "https://chat.example.com", "u1")
        .await
        .unwrap();
    let b = store::store_id(&index, "https://chat.example.com", "u2")
        .await
        .unwrap();
    assert_ne!(a, b);
    assert_eq!(
        store::store_id(&index, "https://chat.example.com", "u1")
            .await
            .unwrap(),
        a
    );
    assert!(!a.contains("example") && a.len() == 32);
    assert_eq!(
        plaintext_anywhere(dir.path(), "chat.example.com"),
        Vec::<String>::new()
    );
}

#[test]
fn stores_stay_off_until_wipes_land() {
    assert!(
        !store::stores_enabled(),
        "the durable-store switch turned on before C5"
    );
}

/// A job that panics stops the store's thread, but the store is released: `close` returns,
/// and the store opens again with its data.
#[tokio::test]
async fn a_panicking_job_releases_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (db, _) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    write_sentinel(&db, 3).await;
    let r: Result<(), StoreError> = db.call(|_| panic!("a bug in a job")).await;
    assert_eq!(r, Err(StoreError::Closed));
    db.close().await;
    let (db, rebuilt) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    assert_eq!(rebuilt, None);
    let users: i64 = db
        .call(|c| c.query_row("SELECT count(*) FROM users", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(users, 3);
}

/// An unreadable check file is not a missing one: nothing is decided, nothing deleted.
#[tokio::test]
async fn an_unreadable_check_deletes_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (db, _) = ready(store::open(dir.path(), Kind::Outbox, "s1", &keys(&slot)));
    db.close().await;
    let check = dir.path().join("outbox.check");
    let before = std::fs::read(dir.path().join("outbox.db")).unwrap();
    std::fs::set_permissions(&check, std::fs::Permissions::from_mode(0o000)).unwrap();
    let r = store::open(dir.path(), Kind::Outbox, "s1", &keys(&slot));
    std::fs::set_permissions(&check, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(r.unwrap_err(), StoreError::Io);
    assert_eq!(std::fs::read(dir.path().join("outbox.db")).unwrap(), before);
}

/// The check was lost (a crash before it was written), but the key still opens the
/// database: it's kept, with its data, and the check is restored.
#[tokio::test]
async fn a_lost_check_with_the_right_key_keeps_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (db, _) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    write_sentinel(&db, 4).await;
    db.close().await;
    std::fs::remove_file(dir.path().join("cache.check")).unwrap();
    let (db, rebuilt) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    assert_eq!(rebuilt, None);
    let users: i64 = db
        .call(|c| c.query_row("SELECT count(*) FROM users", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(users, 4, "a valid database was remade");
    assert!(dir.path().join("cache.check").exists());
}

/// A second path to the same directory (a symlink) is the same store.
#[tokio::test]
async fn an_alias_is_the_same_store() {
    let dir = tempfile::tempdir().unwrap();
    let alias = tempfile::tempdir().unwrap();
    let link = alias.path().join("link");
    std::os::unix::fs::symlink(dir.path(), &link).unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (_db, _) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    assert_eq!(
        store::open(&link, Kind::Cache, "s1", &keys(&slot)).unwrap_err(),
        StoreError::AlreadyOpen
    );
    assert_eq!(
        store::reset(&link, Kind::Cache, "s1", &keys(&slot)).unwrap_err(),
        StoreError::AlreadyOpen
    );
}

const CRASH_DIR: &str = "BROOK_STORE_CRASH_DIR";

/// The child half of the crash test: commit some rows, then die **inside** a live
/// transaction, with a one-page cache so its uncommitted pages have spilled to the WAL.
#[test]
fn crash_child() {
    let Ok(dir) = std::env::var(CRASH_DIR) else {
        return; // only ever runs as the child
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let slot = Arc::new(InMemoryKeySlot::default());
        slot.put("cache:s1", vec![3; 32]);
        let (db, _) = ready(store::open(
            Path::new(&dir),
            Kind::Cache,
            "s1",
            &keys(&slot),
        ));
        write_sentinel(&db, 5).await;
        let _: Result<(), StoreError> = db
            .call(|c| {
                c.execute_batch("PRAGMA cache_size = 1; BEGIN;")?;
                for i in 0..2000 {
                    c.execute(
                        "INSERT INTO users(id, seq, json) VALUES (?1, 1, ?2)",
                        [format!("t{i}"), format!("{SENTINEL}-tx-{i:04}")],
                    )?;
                }
                std::process::abort(); // a kill, mid-transaction: no rollback, no close
            })
            .await;
    });
}

/// A real crash mid-transaction: no plaintext anywhere, the committed rows survive and the
/// uncommitted ones don't.
#[tokio::test]
async fn a_crash_mid_transaction_leaves_only_ciphertext_and_committed_rows() {
    let dir = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["store_tests::crash_child", "--exact", "--test-threads=1"])
        .env(CRASH_DIR, dir.path())
        .output()
        .unwrap()
        .status;
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(6),
        "the child didn't abort: {status:?}"
    );
    let wal = std::fs::metadata(dir.path().join("cache.db-wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    assert!(
        wal > 64 * 1024,
        "the uncommitted pages never reached the WAL ({wal} bytes)"
    );
    assert!(dir.path().join("cache.db").exists());
    assert_eq!(
        plaintext_anywhere(dir.path(), SENTINEL),
        Vec::<String>::new()
    );
    let slot = Arc::new(InMemoryKeySlot::default());
    slot.put("cache:s1", vec![3; 32]);
    let (db, rebuilt) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    assert_eq!(rebuilt, None);
    let users: i64 = db
        .call(|c| c.query_row("SELECT count(*) FROM users", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(users, 5, "committed rows lost or uncommitted ones kept");
}

/// No check and a database the key can't open: damage and a lost key look the same, so
/// nothing is deleted (only an explicit reset clears it).
#[tokio::test]
async fn a_damaged_store_without_its_check_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (db, _) = ready(store::open(dir.path(), Kind::Outbox, "s1", &keys(&slot)));
    db.close().await;
    std::fs::remove_file(dir.path().join("outbox.check")).unwrap();
    let path = dir.path().join("outbox.db");
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[100] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();
    let o = store::open(dir.path(), Kind::Outbox, "s1", &keys(&slot)).unwrap();
    assert!(matches!(o, Opened::Damaged), "{o:?}");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

/// No check, and the slot was gone so a key was made just now: a fresh random key can't be
/// the database's, so it is keyless and remade.
#[tokio::test]
async fn a_lost_slot_and_a_lost_check_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let slot = Arc::new(InMemoryKeySlot::default());
    let (db, _) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    write_sentinel(&db, 2).await;
    db.close().await;
    std::fs::remove_file(dir.path().join("cache.check")).unwrap();
    keys(&slot).destroy("cache:s1").unwrap();
    let (_db, rebuilt) = ready(store::open(dir.path(), Kind::Cache, "s1", &keys(&slot)));
    assert_eq!(rebuilt, Some(Rebuilt::KeyMissing));
}

#[tokio::test]
async fn only_the_owner_can_enter_the_store_directory() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let stores = dir.path().join("stores");
    let slot = Arc::new(InMemoryKeySlot::default());
    let (_db, _) = ready(store::open(&stores, Kind::Cache, "s1", &keys(&slot)));
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&stores), 0o700);
    assert_eq!(mode(&stores.join("cache.check")), 0o600);
}
