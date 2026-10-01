//! The desktop keyring as core's `KeySlot` (staying signed in, #58; spec #46 §3a, §4).
//!
//! One Secret Service item per slot, with the attributes `{application, slot}`, in the
//! user's **existing** default collection. Three Linux facts shape it:
//! - **No prompts, ever.** A missing default collection is not created (creating one asks
//!   for a new keyring password) and a locked one is not unlocked: both are `Unavailable`,
//!   and the user signs in by hand (#46 §5).
//! - **Bounded calls.** Core calls a `KeySlot` inside its session store's write section,
//!   and a D-Bus call to a stuck keyring can wait 25 s. Every call here runs on one
//!   dedicated thread and is answered within [`DEADLINE`], or reported `Unavailable`.
//! - **No uniqueness on the service side.** `CreateItem(replace = false)` happily adds a
//!   second item with the same attributes, so `create` is search-then-create. That's safe
//!   because the app is a unique `GApplication` (a second launch activates the first), and
//!   a `load` that finds more than one item is `Unavailable`, never a guess.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use brook_core::{KeySlot, KeySlotError};

/// How long core waits for any one keyring call.
const DEADLINE: Duration = Duration::from_secs(2);
const APPLICATION: &str = "dev.brook.Brook";

enum Op {
    Probe,
    Load(String),
    Create(String, Vec<u8>),
    Replace(String, Vec<u8>),
    Delete(String),
}

type Reply = Result<Option<Vec<u8>>, KeySlotError>;

/// A `KeySlot` backed by the Secret Service (gnome-keyring, KWallet, oo7-daemon).
pub struct SecretServiceSlot {
    tx: mpsc::Sender<(Op, mpsc::Sender<Reply>)>,
}

impl SecretServiceSlot {
    /// Start the keyring thread. Cheap: it connects on first use.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<(Op, mpsc::Sender<Reply>)>();
        thread::Builder::new()
            .name("brook-keyring".into())
            .spawn(move || worker(rx))
            .expect("spawn the keyring thread");
        Self { tx }
    }

    /// Whether an unlocked default collection answers now (decides if the app offers
    /// staying signed in at all).
    pub fn available(&self) -> bool {
        self.call(Op::Probe).is_ok()
    }

    fn call(&self, op: Op) -> Reply {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send((op, reply_tx))
            .map_err(|_| KeySlotError::Unavailable)?;
        // A stuck keyring answers late or never: the store lock must not wait on it.
        reply_rx
            .recv_timeout(DEADLINE)
            .unwrap_or(Err(KeySlotError::Unavailable))
    }
}

impl KeySlot for SecretServiceSlot {
    fn load(&self, slot: String) -> Result<Option<Vec<u8>>, KeySlotError> {
        self.call(Op::Load(slot))
    }
    fn create(&self, slot: String, bytes: Vec<u8>) -> Result<(), KeySlotError> {
        self.call(Op::Create(slot, bytes)).map(|_| ())
    }
    fn replace(&self, slot: String, bytes: Vec<u8>) -> Result<(), KeySlotError> {
        self.call(Op::Replace(slot, bytes)).map(|_| ())
    }
    fn delete(&self, slot: String) -> Result<(), KeySlotError> {
        self.call(Op::Delete(slot)).map(|_| ())
    }
}

/// The keyring thread: its own runtime and connection, one request at a time.
fn worker(rx: mpsc::Receiver<(Op, mpsc::Sender<Reply>)>) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return; // every call then times out as Unavailable
    };
    // oo7's Service closes its D-Bus session from Drop with tokio::spawn, which needs a
    // runtime context: keep one entered for the thread's life (drops included).
    let _context = runtime.enter();
    let mut service: Option<oo7::dbus::Service> = None;
    for (op, reply) in rx {
        let result = runtime.block_on(async {
            if service.is_none() {
                service = Some(oo7::dbus::Service::new().await.map_err(map_err)?);
            }
            let svc = service.as_ref().expect("just connected");
            let outcome = run(svc, op).await;
            if matches!(outcome, Err(KeySlotError::Fatal(_))) {
                service = None; // reconnect next time (the daemon may have restarted)
            }
            outcome
        });
        // The caller may have given up (deadline). The operation still ran, and later
        // ones run after it in the order issued: core has already fenced a timed-out
        // write or delete, and a sign-out's delete then a new sign-in's replace land in
        // that order. Don't "fix" this by dropping late operations.
        let _ = reply.send(result);
    }
}

async fn run(service: &oo7::dbus::Service, op: Op) -> Reply {
    // The existing default collection only: never create one (that prompts).
    let collection = service
        .with_alias("default")
        .await
        .map_err(map_err)?
        .ok_or(KeySlotError::Unavailable)?;
    // Locked: never unlock (that prompts). The user signs in by hand.
    if collection.is_locked().await.map_err(map_err)? {
        return Err(KeySlotError::Unavailable);
    }
    let attrs = |slot: &str| {
        [
            ("application", APPLICATION.to_string()),
            ("slot", slot.to_string()),
        ]
    };
    match op {
        Op::Probe => Ok(None),
        Op::Load(slot) => {
            let items = collection
                .search_items(&attrs(&slot))
                .await
                .map_err(map_err)?;
            match items.as_slice() {
                [] => Ok(None),
                [item] => Ok(Some(
                    item.secret().await.map_err(map_err)?.as_bytes().to_vec(),
                )),
                _ => Err(KeySlotError::Unavailable), // two keys for one slot: never guess
            }
        }
        Op::Create(slot, bytes) => {
            let items = collection
                .search_items(&attrs(&slot))
                .await
                .map_err(map_err)?;
            if !items.is_empty() {
                return Err(KeySlotError::Exists);
            }
            collection
                .create_item(&label(&slot), &attrs(&slot), bytes, false, None)
                .await
                .map_err(map_err)?;
            Ok(None)
        }
        Op::Replace(slot, bytes) => {
            // Duplicates (another process's race) would make `replace` ambiguous: clear
            // them first, then write the one item.
            let items = collection
                .search_items(&attrs(&slot))
                .await
                .map_err(map_err)?;
            if items.len() > 1 {
                for item in &items {
                    item.delete(None).await.map_err(map_err)?;
                }
            }
            collection
                .create_item(&label(&slot), &attrs(&slot), bytes, true, None)
                .await
                .map_err(map_err)?;
            Ok(None)
        }
        Op::Delete(slot) => {
            for item in collection
                .search_items(&attrs(&slot))
                .await
                .map_err(map_err)?
            {
                item.delete(None).await.map_err(map_err)?;
            }
            Ok(None)
        }
    }
}

/// What the keyring's item list shows; never secret material.
fn label(slot: &str) -> String {
    let kind = slot.split(':').next().unwrap_or("key");
    match kind {
        "session" => "Brook sign-in".into(),
        "cache" | "outbox" => "Brook local data key".into(),
        _ => "Brook key".into(),
    }
}

/// Fixed numeric codes for failures; the backend's text never leaves (it can name items).
fn map_err(err: oo7::dbus::Error) -> KeySlotError {
    use oo7::dbus::Error as E;
    match err {
        // Gone, dismissed or missing: not a failure of the data, just not usable now.
        E::Deleted | E::Dismissed | E::NotFound(_) => KeySlotError::Unavailable,
        E::ZBus(_) => KeySlotError::Fatal(1),
        E::Service(_) => KeySlotError::Fatal(2),
        E::IO(_) => KeySlotError::Fatal(3),
        E::Crypto(_) => KeySlotError::Fatal(4),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_never_carry_the_slot_id() {
        assert_eq!(label("session:https://chat.example.com"), "Brook sign-in");
        assert_eq!(label("cache:7f3c"), "Brook local data key");
        assert!(!label("outbox:secret-id").contains("secret-id"));
    }

    /// A worker that never answers (a stuck keyring) is reported within the deadline.
    #[test]
    fn a_stuck_keyring_is_unavailable_within_the_deadline() {
        let (tx, rx) = mpsc::channel::<(Op, mpsc::Sender<Reply>)>();
        let _held = thread::spawn(move || {
            let _keep = rx.recv(); // take the request, never reply
            thread::sleep(Duration::from_secs(10));
        });
        let slot = SecretServiceSlot { tx };
        let started = std::time::Instant::now();
        assert_eq!(
            slot.load("session:x".into()),
            Err(KeySlotError::Unavailable)
        );
        assert!(started.elapsed() < DEADLINE + Duration::from_millis(500));
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Against the real desktop keyring: `cargo test -p brook-gnome -- --ignored keyring_live`.
    /// Uses a throwaway slot and deletes it; reports and skips when no unlocked keyring answers.
    #[test]
    #[ignore = "touches the desktop keyring"]
    fn keyring_live_round_trip() {
        let slot = SecretServiceSlot::new();
        if !slot.available() {
            eprintln!("no unlocked keyring: skipped");
            return;
        }
        let name = format!("brook-selftest:{}", std::process::id());
        assert_eq!(slot.load(name.clone()), Ok(None));
        slot.create(name.clone(), b"first".to_vec()).unwrap();
        assert_eq!(
            slot.create(name.clone(), b"again".to_vec()),
            Err(KeySlotError::Exists)
        );
        assert_eq!(slot.load(name.clone()), Ok(Some(b"first".to_vec())));
        slot.replace(name.clone(), b"second".to_vec()).unwrap();
        assert_eq!(slot.load(name.clone()), Ok(Some(b"second".to_vec())));
        slot.delete(name.clone()).unwrap();
        assert_eq!(slot.load(name), Ok(None));
    }
}

#[cfg(test)]
mod live_support {
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use brook_core::{KeySlot, KeySlotError};

    use super::SecretServiceSlot;

    /// The real desktop keyring, but under names of this run's own, so a live test can never
    /// touch (or sign out) the developer's real app: the app's slots (`session:<origin>`,
    /// `index`, `cache:<id>`, `outbox:<id>`) all live under a `brook-selftest-<pid>-<time>/`
    /// prefix here, and every slot written is deleted by [`ScopedSlot::purge`].
    pub struct ScopedSlot {
        inner: SecretServiceSlot,
        prefix: String,
        used: Mutex<BTreeSet<String>>,
    }

    impl ScopedSlot {
        /// `None` without an unlocked keyring (the test is then skipped).
        pub fn new() -> Option<Arc<Self>> {
            let inner = SecretServiceSlot::new();
            if !inner.available() {
                return None;
            }
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            Some(Arc::new(Self {
                inner,
                prefix: format!("brook-selftest-{}-{nanos}/", std::process::id()),
                used: Mutex::default(),
            }))
        }

        fn scoped(&self, slot: &str) -> String {
            format!("{}{slot}", self.prefix)
        }

        fn record(&self, slot: &str) {
            self.used.lock().unwrap().insert(slot.to_string());
        }

        /// Delete every slot this run wrote.
        pub fn purge(&self) {
            let used = std::mem::take(&mut *self.used.lock().unwrap());
            for slot in used {
                let _ = self.inner.delete(self.scoped(&slot));
            }
        }

        /// How many slots this run still holds (the cleanup test).
        pub fn held(&self) -> usize {
            self.used.lock().unwrap().len()
        }
    }

    impl KeySlot for ScopedSlot {
        fn load(&self, slot: String) -> Result<Option<Vec<u8>>, KeySlotError> {
            self.inner.load(self.scoped(&slot))
        }
        fn create(&self, slot: String, bytes: Vec<u8>) -> Result<(), KeySlotError> {
            self.record(&slot);
            self.inner.create(self.scoped(&slot), bytes)
        }
        fn replace(&self, slot: String, bytes: Vec<u8>) -> Result<(), KeySlotError> {
            self.record(&slot);
            self.inner.replace(self.scoped(&slot), bytes)
        }
        fn delete(&self, slot: String) -> Result<(), KeySlotError> {
            self.inner.delete(self.scoped(&slot))
        }
    }

    /// A live test's scratch: the slots it wrote and its temp dir, removed even when an
    /// assertion fails.
    pub struct Cleanup {
        pub slot: Arc<ScopedSlot>,
        pub dir: tempfile::TempDir,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.slot.purge();
        }
    }

    /// What a scan of a data dir found.
    #[derive(Debug, Default)]
    pub struct Scan {
        /// Every file read, by path relative to the scanned dir.
        pub files: Vec<String>,
        /// Files containing the needle, or starting as a plain SQLite database.
        pub readable: Vec<String>,
        /// Files that couldn't be read (a scan that skipped them proves nothing).
        pub unreadable: Vec<String>,
    }

    impl Scan {
        pub fn has_file(&self, suffix: &str) -> bool {
            self.files.iter().any(|f| f.ends_with(suffix))
        }
    }

    /// Scan every file under `dir` (SQLite's `-wal` and `-shm` included) for `needle`.
    pub fn scan(dir: &Path, needle: &str) -> Scan {
        fn walk(root: &Path, dir: &Path, needle: &str, out: &mut Scan) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                out.unreadable.push(dir.display().to_string());
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                if path.is_dir() {
                    walk(root, &path, needle, out);
                    continue;
                }
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        let has =
                            |n: &[u8]| !n.is_empty() && bytes.windows(n.len()).any(|w| w == n);
                        if has(needle.as_bytes()) || bytes.starts_with(b"SQLite format 3\0") {
                            out.readable.push(name.clone());
                        }
                        out.files.push(name);
                    }
                    Err(_) => out.unreadable.push(name),
                }
            }
        }
        let mut out = Scan::default();
        walk(dir, dir, needle, &mut out);
        out
    }

    #[test]
    fn the_scan_finds_plain_text_and_plain_sqlite_and_reports_unreadable_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/notes.txt"), b"xx NEEDLE yy").unwrap();
        std::fs::write(dir.path().join("plain.db"), b"SQLite format 3\0rest").unwrap();
        std::fs::write(dir.path().join("cipher.db"), [7u8; 64]).unwrap();
        let found = scan(dir.path(), "NEEDLE");
        assert_eq!(found.files.len(), 3);
        let mut readable = found.readable.clone();
        readable.sort();
        assert_eq!(readable, ["plain.db", "sub/notes.txt"]);
        assert!(found.unreadable.is_empty());
        assert!(found.has_file("cipher.db") && !found.has_file("other.db"));
        // A dangling symlink can't be read: reported, not skipped.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/nonexistent/x", dir.path().join("dangling")).unwrap();
            assert_eq!(scan(dir.path(), "NEEDLE").unreadable, ["dangling"]);
        }
    }

    #[test]
    #[ignore = "touches the desktop keyring (under its own names)"]
    fn a_scoped_slot_keeps_its_names_apart_and_forgets_them_on_purge() {
        let Some(slot) = ScopedSlot::new() else {
            eprintln!("no unlocked keyring: skipped");
            return;
        };
        slot.create("session:https://h".into(), b"a".to_vec())
            .unwrap();
        slot.replace("index".into(), b"b".to_vec()).unwrap();
        assert_eq!(slot.held(), 2);
        assert_eq!(slot.load("index".into()), Ok(Some(b"b".to_vec())));
        // The unscoped (the app's real) name is a different slot.
        assert_eq!(slot.inner.load("index".into()), Ok(None));
        slot.purge();
        assert_eq!(slot.held(), 0);
        assert_eq!(slot.load("index".into()), Ok(None));
    }
}

#[cfg(test)]
mod live_restore {
    use std::sync::Arc;

    use brook_core::{BrookClient, CoreConfig, LoginOutcome, RestoreOutcome};

    use super::live_support::{Cleanup, ScopedSlot};

    /// Stay signed in end to end: the real keyring (under this run's own names, never the
    /// app's) + core's restore against a server. `BROOK_LIVE_SERVER=... BROOK_LIVE_HANDLE=...
    /// BROOK_LIVE_PASSWORD=... cargo test -p brook-gnome -- --ignored keyring_restore_live`
    #[test]
    #[ignore = "touches the desktop keyring and a live server"]
    fn keyring_restore_live() {
        let (Ok(server), Ok(handle), Ok(password)) = (
            std::env::var("BROOK_LIVE_SERVER"),
            std::env::var("BROOK_LIVE_HANDLE"),
            std::env::var("BROOK_LIVE_PASSWORD"),
        ) else {
            eprintln!("BROOK_LIVE_* not set: skipped");
            return;
        };
        let Some(slot) = ScopedSlot::new() else {
            eprintln!("no unlocked keyring: skipped");
            return;
        };
        let scratch = Cleanup {
            slot: slot.clone(),
            dir: tempfile::tempdir().unwrap(),
        };
        let dir = scratch.dir.path().to_path_buf();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = || {
            let c = BrookClient::new(CoreConfig::with_options(&server, true).unwrap()).unwrap();
            c.enable_persistence(slot.clone(), dir.clone());
            Arc::new(c)
        };
        rt.block_on(async {
            let first = client();
            assert!(matches!(
                first.login(&handle, &password).await.unwrap(),
                LoginOutcome::LoggedIn(_)
            ));
            drop(first); // quit, not sign-out
            let again = client();
            let restored = again.restore().await;
            println!("restore after quit -> {restored:?}");
            assert!(matches!(restored, RestoreOutcome::LoggedIn(_)));
            again.logout().await;
            println!("sign_out_complete -> {}", again.sign_out_complete());
            drop(again);
            let after = client().restore().await;
            println!("restore after sign-out -> {after:?}");
            assert_eq!(after, RestoreOutcome::NotSignedIn);
        });
    }
}

#[cfg(test)]
mod live_local_data {
    use std::sync::Arc;
    use std::time::Duration;

    use brook_core::{
        BrookClient, CoreConfig, FileCacheState, LoginOutcome, OutgoingFile, TransferId,
    };

    use super::live_support::{scan, Cleanup, Scan, ScopedSlot};

    struct Live {
        server: String,
        handle: String,
        password: String,
        channel: String,
    }

    fn live() -> Option<(Live, Cleanup)> {
        let (Ok(server), Ok(handle), Ok(password), Ok(channel)) = (
            std::env::var("BROOK_LIVE_SERVER"),
            std::env::var("BROOK_LIVE_HANDLE"),
            std::env::var("BROOK_LIVE_PASSWORD"),
            std::env::var("BROOK_LIVE_CHANNEL"),
        ) else {
            eprintln!("BROOK_LIVE_* not set: skipped");
            return None;
        };
        let Some(slot) = ScopedSlot::new() else {
            eprintln!("no unlocked keyring: skipped");
            return None;
        };
        let dir = tempfile::tempdir().unwrap();
        Some((
            Live {
                server,
                handle,
                password,
                channel,
            },
            Cleanup { slot, dir },
        ))
    }

    /// A signed-in client with local data on, in `scratch`'s own dir and keyring names.
    async fn signed_in(live: &Live, scratch: &Cleanup) -> Arc<BrookClient> {
        let data_dir = scratch.dir.path().join("data");
        let client = Arc::new(
            BrookClient::new(CoreConfig::with_options(&live.server, true).unwrap()).unwrap(),
        );
        client.enable_persistence(scratch.slot.clone(), data_dir.clone());
        assert!(
            client
                .enable_local_data(scratch.slot.clone(), data_dir)
                .await,
            "local data should turn on with a usable keyring"
        );
        assert!(matches!(
            client.login(&live.handle, &live.password).await.unwrap(),
            LoginOutcome::LoggedIn(_)
        ));
        client.start_realtime().await.unwrap();
        client
    }

    /// What a good scan looks like: both stores' databases were read, nothing was unreadable,
    /// and nothing was readable.
    fn assert_ciphertext(found: &Scan, when: &str) {
        println!("{when}: scanned {:?}", found.files);
        assert!(
            found.unreadable.is_empty(),
            "{when}: unreadable {:?}",
            found.unreadable
        );
        assert!(
            found.has_file("cache.db"),
            "{when}: no cache.db scanned: {:?}",
            found.files
        );
        assert!(
            found.has_file("outbox.db"),
            "{when}: no outbox.db scanned: {:?}",
            found.files
        );
        assert!(
            found.readable.is_empty(),
            "{when}: readable on disk: {:?}",
            found.readable
        );
    }

    /// Offline cache at rest, end to end on the real desktop keyring (under this run's own
    /// names): with local data on, a message that went through the cache (and the outbox)
    /// leaves nothing readable on disk, scanned while the stores are open (their `-wal` and
    /// `-shm` exist then) and again after they close.
    /// `BROOK_LIVE_SERVER=... BROOK_LIVE_HANDLE=... BROOK_LIVE_PASSWORD=... BROOK_LIVE_CHANNEL=<id>
    /// cargo test -p brook-gnome -- --ignored local_data_is_ciphertext_on_disk`
    #[test]
    #[ignore = "touches the desktop keyring and a live server"]
    fn local_data_is_ciphertext_on_disk() {
        let Some((live, scratch)) = live() else {
            return;
        };
        let marker = format!("CIPHERCHECK-{}-needle", std::process::id());
        let outbox_marker = format!("{marker}-outbox");
        let data_dir = scratch.dir.path().join("data");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let while_open = rt.block_on(async {
            let client = signed_in(&live, &scratch).await;
            client
                .send_message(&live.channel, &marker, None)
                .await
                .unwrap();
            client
                .send_queued(&live.channel, &outbox_marker, None, None)
                .await
                .unwrap();
            // Wait until both went through the cache, then read them back from it.
            let mut seen = false;
            for _ in 0..40 {
                if let Ok(page) = client.cached_messages(&live.channel, None, 50).await {
                    let bodies: Vec<&str> = page.messages.iter().map(|m| m.body.as_str()).collect();
                    if bodies.contains(&marker.as_str()) && bodies.contains(&outbox_marker.as_str())
                    {
                        seen = true;
                        break;
                    }
                }
                let _ = client.load_head(&live.channel, 50).await;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            assert!(seen, "both messages should be readable from the cache");
            // Still open: SQLite's write-ahead logs exist and can hold the newest pages.
            let open = scan(&data_dir, &marker);
            client.logout().await;
            client.close_local_data().await;
            open
        });
        assert!(
            while_open.has_file("-wal"),
            "the open stores should have write-ahead logs: {:?}",
            while_open.files
        );
        assert_ciphertext(&while_open, "while open");
        assert_ciphertext(&scan(&data_dir, &marker), "after close");
        assert!(scratch.slot.held() > 0, "the run did write keyring slots");
    }

    /// Attachments end to end on the real keyring (under this run's own names) and a real
    /// server: a file goes out through the outbox, is kept available offline, and is then
    /// saved from the cache (which never touches the network), byte for byte, while the
    /// data dir holds none of it. Same `BROOK_LIVE_*` as above.
    /// cargo test -p brook-gnome -- --ignored attachment_is_kept_offline_and_ciphertext
    #[test]
    #[ignore = "touches the desktop keyring and a live server"]
    fn attachment_is_kept_offline_and_ciphertext() {
        let Some((live, scratch)) = live() else {
            return;
        };
        // RUST_LOG=brook_core=debug shows why core refused something.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        let pid = std::process::id();
        // 3 MiB: more than one encrypted chunk (1 MiB each), with a marker repeated through it.
        let marker = format!("FILECHECK-{pid}-needle");
        let mut content = Vec::new();
        while content.len() < 3 * 1024 * 1024 {
            content.extend_from_slice(marker.as_bytes());
            content.extend_from_slice(&(content.len() as u64).to_le_bytes());
        }
        let source = scratch.dir.path().join("source.bin");
        std::fs::write(&source, &content).unwrap();
        let caption = format!("attachment-check-{pid}");
        let data_dir = scratch.dir.path().join("data");
        let target = scratch.dir.path().join("saved.bin");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let while_open = rt.block_on(async {
            let client = signed_in(&live, &scratch).await;
            let channel = &live.channel;
            client
                .send_queued_with_files(
                    channel,
                    &caption,
                    None,
                    None,
                    vec![OutgoingFile {
                        path: source.clone(),
                        filename: "source.bin".into(),
                        content_type: "application/octet-stream".into(),
                        transfer_id: Some(TransferId::new()),
                    }],
                )
                .await
                .unwrap();
            // The server has it: its message carries the file.
            let mut file_id = None;
            for _ in 0..60 {
                let history = client.channel_history(channel, None).await.unwrap();
                if let Some(file) = history
                    .iter()
                    .find(|m| m.body == caption)
                    .and_then(|m| m.attachments.first())
                {
                    assert_eq!(file.size, content.len() as u64, "the server's size");
                    file_id = Some(file.id.clone());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let file_id = file_id.expect("the file should reach the server");
            // The cache indexes a file through its message: wait until it holds this one.
            let mut cached = false;
            for _ in 0..40 {
                if let Ok(page) = client.cached_messages(channel, None, 50).await {
                    if page.messages.iter().any(|m| m.body == caption) {
                        cached = true;
                        break;
                    }
                }
                let _ = client.load_head(channel, 50).await;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            assert!(cached, "the message should reach the cache");
            // Keep it offline, and wait until it is complete in the cache.
            client.pin_file(&file_id).await.unwrap();
            let mut pinned = false;
            for _ in 0..80 {
                if let Ok(FileCacheState::Pinned { cached: true, .. }) =
                    client.file_state(&file_id).await
                {
                    pinned = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            assert!(pinned, "the pinned file should be complete in the cache");
            // Saving from the cache: no network involved, and the bytes are the original's.
            assert!(client.save_cached_file(&file_id, &target).await.unwrap());
            let open = scan(&data_dir, &marker);
            client.logout().await;
            client.close_local_data().await;
            open
        });
        let saved = std::fs::read(&target).unwrap();
        assert_eq!(saved.len(), content.len());
        assert!(saved == content, "the saved copy differs from the original");
        // Nothing the cache wrote holds the file's bytes or a plain database, open or closed.
        assert_ciphertext(&while_open, "while open");
        assert_ciphertext(&scan(&data_dir, &marker), "after close");
    }
}
