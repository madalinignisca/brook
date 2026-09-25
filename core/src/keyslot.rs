//! Key storage for local data (spec docs/superpowers/specs/2026-09-25-client-local-data-encryption-design.md
//! §3a, plan 2026-09-25-keyslot-session-plan.md P1).
//!
//! A platform only stores bytes under a named slot (the data-protection Keychain on Apple, the
//! Secret Service or portal keyring on Linux); core makes, uses and wipes the keys. A slot that
//! doesn't exist is **absent**, which is the only state that ever leads to a new key: locked or
//! failing storage is an error, and nothing is created, replaced or deleted because of it.

use std::collections::HashMap;
use std::sync::Mutex;

use zeroize::Zeroizing;

/// Why a slot operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeySlotError {
    /// `create` found the slot already taken (another process won the race): load it.
    #[error("the slot already exists")]
    Exists,
    /// The store is locked or not answering (a device locked since boot, a locked keyring):
    /// keep everything and try later.
    #[error("key storage is unavailable")]
    Unavailable,
    /// A fault that retrying won't fix (an entitlement or signing problem). A numeric status
    /// only: backend text never travels, so it can't carry anything sensitive.
    #[error("key storage failed (status {0})")]
    Fatal(i32),
}

/// Named-slot byte storage, implemented by the platform. Synchronous: callers run it inside
/// their own critical sections (the session store's write section), so **every call must
/// return within a few seconds or report `Unavailable`**. A stuck or prompting store (a D-Bus
/// keyring can block for 25 s) must be cut off by the implementation with its own deadline;
/// core doesn't move these calls off the async worker (`block_in_place` panics on the
/// current-thread runtimes core runs on in tests and some embedders).
///
/// `load` finding more than one item for a slot is `Unavailable`, never a guess.
pub trait KeySlot: Send + Sync {
    /// The bytes under `slot`, `None` if the slot doesn't exist.
    fn load(&self, slot: String) -> Result<Option<Vec<u8>>, KeySlotError>;
    /// Create-only: `Exists` if the slot is taken. Never overwrites.
    fn create(&self, slot: String, bytes: Vec<u8>) -> Result<(), KeySlotError>;
    /// An atomic overwrite (or create when absent).
    fn replace(&self, slot: String, bytes: Vec<u8>) -> Result<(), KeySlotError>;
    /// Remove the slot; removing an absent slot is fine.
    fn delete(&self, slot: String) -> Result<(), KeySlotError>;
}

/// A 256-bit key, wiped from memory when dropped, never printed.
pub struct Key(Zeroizing<[u8; 32]>);

impl Key {
    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

/// Stored bytes as a key; anything but 32 bytes is a fault, never silently used.
fn key_from(bytes: Vec<u8>) -> Result<Key, KeySlotError> {
    let bytes = Zeroizing::new(bytes);
    let array: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| KeySlotError::Fatal(-1))?;
    Ok(Key(Zeroizing::new(array)))
}

/// Keys over a `KeySlot`: one random key per slot, made only when the slot is absent.
pub struct KeyStore<S: KeySlot + ?Sized> {
    slots: std::sync::Arc<S>,
}

impl<S: KeySlot + ?Sized> KeyStore<S> {
    pub fn new(slots: std::sync::Arc<S>) -> Self {
        Self { slots }
    }

    /// The key in `slot`, making one if (and only if) the slot is absent.
    pub fn get_or_create(&self, slot: &str) -> Result<Key, KeySlotError> {
        self.get_or_create_reporting(slot).map(|(key, _)| key)
    }

    /// Like `get_or_create`, and whether this call made the key (`true`) or found one. A key
    /// made just now can't be the key of anything already on disk; a key another creator
    /// won the race with counts as found.
    pub fn get_or_create_reporting(&self, slot: &str) -> Result<(Key, bool), KeySlotError> {
        if let Some(bytes) = self.slots.load(slot.to_string())? {
            return key_from(bytes).map(|k| (k, false));
        }
        // Absent: the one state that makes a key. An entropy failure is an error, never a
        // weaker key.
        let mut fresh = Zeroizing::new([0u8; 32]);
        getrandom::fill(fresh.as_mut()).map_err(|_| KeySlotError::Unavailable)?;
        match self.slots.create(slot.to_string(), fresh.to_vec()) {
            Ok(()) => Ok((Key(fresh), true)),
            // Another process made it between our load and our create: use theirs. If that
            // reload finds nothing (or can't read), don't make another one.
            Err(KeySlotError::Exists) => match self.slots.load(slot.to_string())? {
                Some(bytes) => key_from(bytes).map(|k| (k, false)),
                None => Err(KeySlotError::Unavailable),
            },
            Err(err) => Err(err),
        }
    }

    /// Destroy `slot`'s key (crypto-erase of what it encrypts).
    pub fn destroy(&self, slot: &str) -> Result<(), KeySlotError> {
        self.slots.delete(slot.to_string())
    }
}

/// An in-memory `KeySlot`: tests, and runs without secure storage (a second instance, a build
/// without keychain access). Failures can be scripted per operation.
#[derive(Default)]
pub struct InMemoryKeySlot {
    slots: Mutex<HashMap<String, Vec<u8>>>,
    fail_next: Mutex<Vec<(&'static str, KeySlotError)>>,
}

impl InMemoryKeySlot {
    /// The next call of `op` (`load`, `create`, `replace`, `delete`) fails with `err`.
    pub fn fail_next(&self, op: &'static str, err: KeySlotError) {
        self.fail_next.lock().unwrap().push((op, err));
    }

    /// Put bytes in a slot directly (tests: another process wrote it).
    pub fn put(&self, slot: &str, bytes: Vec<u8>) {
        self.slots.lock().unwrap().insert(slot.to_string(), bytes);
    }

    pub fn contains(&self, slot: &str) -> bool {
        self.slots.lock().unwrap().contains_key(slot)
    }

    fn scripted(&self, op: &str) -> Result<(), KeySlotError> {
        let mut fails = self.fail_next.lock().unwrap();
        match fails.iter().position(|(o, _)| *o == op) {
            Some(i) => Err(fails.remove(i).1),
            None => Ok(()),
        }
    }
}

impl KeySlot for InMemoryKeySlot {
    fn load(&self, slot: String) -> Result<Option<Vec<u8>>, KeySlotError> {
        self.scripted("load")?;
        Ok(self.slots.lock().unwrap().get(&slot).cloned())
    }
    fn create(&self, slot: String, bytes: Vec<u8>) -> Result<(), KeySlotError> {
        self.scripted("create")?;
        let mut slots = self.slots.lock().unwrap();
        if slots.contains_key(&slot) {
            return Err(KeySlotError::Exists);
        }
        slots.insert(slot, bytes);
        Ok(())
    }
    fn replace(&self, slot: String, bytes: Vec<u8>) -> Result<(), KeySlotError> {
        self.scripted("replace")?;
        self.slots.lock().unwrap().insert(slot, bytes);
        Ok(())
    }
    fn delete(&self, slot: String) -> Result<(), KeySlotError> {
        self.scripted("delete")?;
        self.slots.lock().unwrap().remove(&slot);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn store() -> (Arc<InMemoryKeySlot>, KeyStore<InMemoryKeySlot>) {
        let slots = Arc::new(InMemoryKeySlot::default());
        (slots.clone(), KeyStore::new(slots))
    }

    #[test]
    fn an_absent_slot_gets_one_random_key_and_keeps_it() {
        let (slots, keys) = store();
        let first = keys.get_or_create("cache:a").unwrap();
        assert!(slots.contains("cache:a"));
        let again = keys.get_or_create("cache:a").unwrap();
        assert_eq!(
            first.bytes(),
            again.bytes(),
            "a second key replaced the first"
        );
        assert_ne!(first.bytes(), &[0u8; 32], "not random");
        let other = keys.get_or_create("cache:b").unwrap();
        assert_ne!(first.bytes(), other.bytes(), "slots share a key");
    }

    /// Another process created the slot between our load and our create: use its key.
    #[test]
    fn a_lost_creation_race_uses_the_winners_key() {
        let racing = RacingSlot {
            inner: Arc::new(InMemoryKeySlot::default()),
            winner: vec![9; 32],
        };
        let keys = KeyStore::new(Arc::new(racing));
        let key = keys.get_or_create("cache:a").unwrap();
        assert_eq!(key.bytes(), &[9u8; 32]);
    }

    /// Locked or failing storage never makes, replaces or deletes a key.
    #[test]
    fn unavailable_or_fatal_storage_changes_nothing() {
        for err in [KeySlotError::Unavailable, KeySlotError::Fatal(-34018)] {
            let (slots, keys) = store();
            slots.put("cache:a", vec![5; 32]);
            slots.fail_next("load", err.clone());
            assert_eq!(keys.get_or_create("cache:a").unwrap_err(), err);
            assert_eq!(
                slots.load("cache:a".into()).unwrap(),
                Some(vec![5; 32]),
                "{err:?} changed the key"
            );
        }
        let (slots, keys) = store();
        slots.fail_next("load", KeySlotError::Unavailable);
        assert!(keys.get_or_create("cache:a").is_err());
        assert!(
            !slots.contains("cache:a"),
            "a key was made while storage was locked"
        );
    }

    /// A lost race whose reload then finds nothing (or can't read) is not a reason to make
    /// another key.
    #[test]
    fn a_lost_race_followed_by_an_empty_reload_is_unavailable() {
        let slots = Arc::new(InMemoryKeySlot::default());
        slots.fail_next("create", KeySlotError::Exists);
        let keys = KeyStore::new(slots.clone());
        assert_eq!(
            keys.get_or_create("cache:a").unwrap_err(),
            KeySlotError::Unavailable
        );
        assert!(!slots.contains("cache:a"));
    }

    #[test]
    fn destroy_removes_only_its_slot() {
        let (slots, keys) = store();
        keys.get_or_create("cache:a").unwrap();
        keys.get_or_create("cache:b").unwrap();
        keys.destroy("cache:a").unwrap();
        assert!(!slots.contains("cache:a"));
        assert!(slots.contains("cache:b"));
    }

    #[test]
    fn a_key_never_prints() {
        let (_, keys) = store();
        let key = keys.get_or_create("cache:a").unwrap();
        let shown = format!("{key:?}");
        assert_eq!(shown, "Key(<redacted>)");
        assert_eq!(
            KeySlotError::Fatal(-34018).to_string(),
            "key storage failed (status -34018)"
        );
    }

    /// A slot whose first load is empty and whose create loses to a concurrent writer.
    struct RacingSlot {
        inner: Arc<InMemoryKeySlot>,
        winner: Vec<u8>,
    }

    impl KeySlot for RacingSlot {
        fn load(&self, slot: String) -> Result<Option<Vec<u8>>, KeySlotError> {
            self.inner.load(slot)
        }
        fn create(&self, slot: String, _bytes: Vec<u8>) -> Result<(), KeySlotError> {
            self.inner.put(&slot, self.winner.clone()); // the other process got there first
            Err(KeySlotError::Exists)
        }
        fn replace(&self, slot: String, bytes: Vec<u8>) -> Result<(), KeySlotError> {
            self.inner.replace(slot, bytes)
        }
        fn delete(&self, slot: String) -> Result<(), KeySlotError> {
            self.inner.delete(slot)
        }
    }
}
