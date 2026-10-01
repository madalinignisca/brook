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
mod live_restore {
    use std::sync::Arc;

    use brook_core::{BrookClient, CoreConfig, LoginOutcome, RestoreOutcome};

    use super::SecretServiceSlot;

    /// Stay signed in end to end: the real keyring + core's restore against a server.
    /// `BROOK_LIVE_SERVER=... BROOK_LIVE_HANDLE=... BROOK_LIVE_PASSWORD=... cargo test
    /// -p brook-gnome -- --ignored keyring_restore_live`
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
        let slot = Arc::new(SecretServiceSlot::new());
        if !slot.available() {
            eprintln!("no unlocked keyring: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("brook-live-{}", std::process::id()));
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
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod live_local_data {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use brook_core::{BrookClient, CoreConfig, LoginOutcome};

    use super::SecretServiceSlot;

    /// Every file under `dir`, recursively, that contains `needle`, or starts like a plain
    /// SQLite database.
    fn plaintext(dir: &Path, needle: &str, found: &mut Vec<String>, files: &mut usize) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                plaintext(&path, needle, found, files);
            } else if let Ok(bytes) = std::fs::read(&path) {
                *files += 1;
                let has = |n: &[u8]| !n.is_empty() && bytes.windows(n.len()).any(|w| w == n);
                if has(needle.as_bytes()) || bytes.starts_with(b"SQLite format 3\0") {
                    found.push(path.display().to_string());
                }
            }
        }
    }

    #[test]
    fn the_scan_finds_plain_text_and_plain_sqlite_files() {
        let dir = std::env::temp_dir().join(format!("brook-scan-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/notes.txt"), b"xx NEEDLE yy").unwrap();
        std::fs::write(dir.join("plain.db"), b"SQLite format 3\0rest").unwrap();
        std::fs::write(dir.join("cipher.db"), [7u8; 64]).unwrap();
        let (mut found, mut files) = (Vec::new(), 0);
        plaintext(&dir, "NEEDLE", &mut found, &mut files);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(files, 3);
        found.sort();
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(
            found[0].ends_with("plain.db") && found[1].ends_with("notes.txt"),
            "{found:?}"
        );
    }

    /// Offline cache at rest, end to end on the real desktop keyring: with local data on, a
    /// message that went through the cache (and the outbox) leaves nothing readable on disk.
    /// `BROOK_LIVE_SERVER=... BROOK_LIVE_HANDLE=... BROOK_LIVE_PASSWORD=... BROOK_LIVE_CHANNEL=<id>
    /// cargo test -p brook-gnome -- --ignored local_data_is_ciphertext_on_disk`
    #[test]
    #[ignore = "touches the desktop keyring and a live server"]
    fn local_data_is_ciphertext_on_disk() {
        let (Ok(server), Ok(handle), Ok(password), Ok(channel)) = (
            std::env::var("BROOK_LIVE_SERVER"),
            std::env::var("BROOK_LIVE_HANDLE"),
            std::env::var("BROOK_LIVE_PASSWORD"),
            std::env::var("BROOK_LIVE_CHANNEL"),
        ) else {
            eprintln!("BROOK_LIVE_* not set: skipped");
            return;
        };
        let slot = Arc::new(SecretServiceSlot::new());
        if !slot.available() {
            eprintln!("no unlocked keyring: skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("brook-live-data-{}", std::process::id()));
        let marker = format!("CIPHERCHECK-{}-{}", std::process::id(), "needle");
        let outbox_marker = format!("{marker}-outbox");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let client = Arc::new(
                BrookClient::new(CoreConfig::with_options(&server, true).unwrap()).unwrap(),
            );
            client.enable_persistence(slot.clone(), dir.clone());
            assert!(
                client.enable_local_data(slot.clone(), dir.clone()).await,
                "local data should turn on with a usable keyring"
            );
            assert!(matches!(
                client.login(&handle, &password).await.unwrap(),
                LoginOutcome::LoggedIn(_)
            ));
            client.start_realtime().await.unwrap();
            client.send_message(&channel, &marker, None).await.unwrap();
            client
                .send_queued(&channel, &outbox_marker, None, None)
                .await
                .unwrap();
            // Wait until both went through the cache, then read them back from it.
            let mut seen = false;
            for _ in 0..40 {
                if let Ok(page) = client.cached_messages(&channel, None, 50).await {
                    let bodies: Vec<&str> = page.messages.iter().map(|m| m.body.as_str()).collect();
                    if bodies.contains(&marker.as_str()) && bodies.contains(&outbox_marker.as_str())
                    {
                        seen = true;
                        break;
                    }
                }
                let _ = client.load_head(&channel, 50).await;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            assert!(seen, "both messages should be readable from the cache");
            client.logout().await;
            client.close_local_data().await;
        });
        let (mut found, mut files) = (Vec::new(), 0);
        plaintext(&dir, &marker, &mut found, &mut files);
        println!("scanned {files} files under {}", dir.display());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(files > 0, "the stores should have written files");
        assert!(found.is_empty(), "readable on disk: {found:?}");
    }
}
