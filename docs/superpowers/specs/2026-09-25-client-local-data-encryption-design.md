# Client local data encryption

Status: draft for review (owner approved the direction 2026-09-25). Owners: core +
the Apple and Linux clients. The server is not involved.

## 1. Problem

Someone evaluating Brook asked whether the apps keep their local data encrypted,
readable only by Brook. Today the apps store very little: the Mac keeps a session,
and nothing caches messages yet. That makes now the right time to decide, before
a message cache or attachments land on disk.

The owner's constraints:
- OS secret stores that pop up "allow access?" dialogs are not a protection:
  people click Allow.
- Deriving the key from the account password couples local data to password
  changes and to other devices. Rejected (see §6).
- It must also work, and say honestly what it can and cannot do, on Linux
  without any secret service (e.g. a bare sway session).

## 2. Threat model

**Protects against:**
- other apps reading Brook's files (where the OS can isolate apps: §4);
- backups and file-sync tools copying readable chats and tokens off the device;
- someone browsing the disk of a powered-off or logged-out machine, when combined
  with the OS's disk encryption.

**Does not protect against, and the UI must not imply otherwise:**
- malware running as the same user **outside** a sandbox (a native Linux build,
  Windows later): such code can read the app's memory or wait for the key;
- an unlocked, logged-in session in someone else's hands (a later optional app
  lock, §7, covers this);
- root / an administrator.

## 3. Design: a per-device data key, held by the OS

1. On first run each install generates a random 256-bit **data key**. Everything
   Brook writes locally is encrypted with it: the session (refresh token), and
   later the message cache (SQLCipher or AES-256-GCM per record; core decides)
   and cached attachments.
2. The data key never leaves the device and is never derived from the account
   password. **Password changes do not touch local encryption**; being signed
   out by one just means signing in again.
3. The data key is stored by the platform's per-app secure storage (§4). Brook
   never shows its own "allow access" prompt, and never relies on one.
4. **Local data is disposable.** The server is the source of truth. If the key
   is **missing** (reinstall, wiped keychain, restored backup on a new machine),
   the app deletes its local store, makes a new key, signs in again and re-syncs.
   Nothing is ever pulled from other devices.
   - **Missing and unreadable are different.** Only "the item does not exist"
     (`errSecItemNotFound` on Apple; no item on Linux) means lost. These never
     delete anything:
     - `errSecInteractionNotAllowed`: the device is locked before its first unlock;
       on iOS a background or push launch meets this routinely;
     - `errSecMissingEntitlement` (-34018): a signing or build fault, reported
       loudly;
     - a Secret Service that is locked or not answering;
     - any other error.
     The app then runs **online-only** (no local store opened) and tries again on
     the next launch or unlock. Otherwise a locked phone would lose its cache on every
     early wake.
5. **Crypto-erase on every wipe.** Any wipe (a missing key, sign-out with "Remove
   this device's data", a different user signing in) deletes the key **first**, then
   the files, and makes a new key. Anything a file delete misses (SQLite free pages,
   journals, filesystem snapshots) stays unreadable.
6. **The session (stay signed in, #58).** On Apple the refresh token is its own
   data-protection Keychain item (same attributes as §4, service
   `dev.brook.Brook.session`); encrypting it again with a key from the same Keychain
   would add nothing. On Linux it lives in the encrypted local store. Sign-out and a
   remote sign-out delete it.

## 3a. Core interface

Core owns the key's use (generation, derivation, encryption, zeroizing); a platform
only stores 32 bytes. One synchronous foreign trait over UniFFI (not async: async
foreign traits force the generated Swift into Swift 5 mode), implemented in Swift for
Apple and natively in Rust (`oo7`) for Linux:

```rust
pub trait KeySlot: Send + Sync {
    /// The stored key, None if there is none (then core makes one), or an error.
    fn load(&self) -> Result<Option<Vec<u8>>, KeySlotError>;
    /// Create-only: if an item already exists (another process won), return
    /// `Exists` and core loads that one. Never overwrites.
    fn store(&self, key: Vec<u8>) -> Result<(), KeySlotError>;
    fn delete(&self) -> Result<(), KeySlotError>;
}
pub enum KeySlotError {
    Exists,        // store() lost a race: load again
    Unavailable,   // locked or not answering: keep everything, run online-only
    Fatal(String), // e.g. -34018: report, keep everything, run online-only
}
```

- Core generates the key with `getrandom` and derives per-use subkeys with
  HKDF-SHA256 (per store and per purpose, e.g. `brook.cache.v1 | origin | user id`),
  so one device key serves every store. It zeroizes key material after use.
- Tests run on an in-memory `KeySlot`. The Apple test host has no team signature, so
  the data-protection keychain answers -34018 there.

## 4. Where the key lives, per platform (strongest first)

| Platform | Store | Isolation from other apps | User prompts |
|---|---|---|---|
| macOS / iOS | Data-protection Keychain (`kSecUseDataProtectionKeychain`): `kSecClassGenericPassword`, service `dev.brook.Brook.datakey`, `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`, `kSecAttrSynchronizable: false` set explicitly on both add and query, shared access group `$(TeamID).dev.brook.shared` (so a future notification or share extension can read it without a migration) | **Yes**, enforced by code signature | None |
| Linux, Flatpak | Secret portal (`org.freedesktop.portal.Secret`): a per-app secret handed only to this app ID; data key derived from it with HKDF | **Yes**, per app ID | None |
| Linux, native, with a Secret Service (gnome-keyring, KWallet) | Secret Service item tagged for Brook | **No**: any app in the session can read it | Usually none; it's unlocked at login |
| Linux, no secret service (bare sway, etc.) | see §5 | see §5 | see §5 |
| Windows (later) | DPAPI, user scope | No (user-scoped, not app-scoped) | None |

Apple details:
- **Development builds:** the data-protection keychain needs a team-signed build.
  Debug is signed with the dev team (`Local.xcconfig`); an unsigned build gets -34018,
  which is `Fatal` and never a wipe.
- **Backups:** the local store's directory is excluded from backups
  (`isExcludedFromBackup`). The key is ThisDeviceOnly anyway, so a backed-up store
  would be unreadable ciphertext.

In Rust, the `oo7` crate speaks both the Secret portal (inside Flatpak) and the
Secret Service (outside), so core can own one Linux backend. Apple needs a small
Swift keychain shim behind a core callback.

Two traps (Linux review):
- **Select the backend explicitly; never let it fall back.** Inside Flatpak,
  `oo7::Keyring::new()` silently falls back to the D-Bus Secret Service when the
  portal is missing, which would quietly downgrade "isolated" to "readable by
  other apps". Core picks the portal backend under Flatpak and treats "no portal"
  as §5. The Flatpak manifest must also **not** grant
  `--talk-name=org.freedesktop.secrets`, so a fallback is impossible even by bug.
- **One construction, not two.** Use oo7's portal-backed file keyring *or* the
  HKDF-from-portal-secret construction above, not both. Core chooses when it
  implements (proposed: oo7's file keyring, so no hand-rolled key derivation).

**Flatpak is the Linux configuration that actually delivers app isolation.** The
native tarball (#38) works, but the settings screen says "Protected from: backups,
file browsing. Not from: other apps you run."

## 5. No secret service at all (bare sway)

Both need a backend running. The Secret Service is provided by gnome-keyring,
KWallet or the standalone `oo7-daemon`; the Secret **portal** by gnome-keyring or
`oo7-portal` (KWallet as a portal backend is unverified, so don't rely on it).
On a bare sway session often none runs, and a Flatpak's portal then has nothing
to hand out.

Detection probes **capability**, not the desktop name (`XDG_CURRENT_DESKTOP` says
nothing about what runs): try the portal (under Flatpak) or the Secret Service.
A service that is present but **locked** counts as unavailable: on sway,
gnome-keyring started by D-Bus activation can pop its own unlock prompt, which is
exactly the prompt-driven store the owner does not want to rely on.

Brook detects this on first run and **asks once, never silently degrading**:

1. **Local passphrase (recommended here).** The user picks a passphrase used only
   on this device. It has nothing to do with the account password. The data key is
   wrapped with Argon2id(passphrase), and Brook asks for the passphrase when it
   starts: effectively an app lock. Forgetting it costs only the local copy (§3.4).
2. **No protection.** The data key sits in a 0600 file in
   `$XDG_DATA_HOME/brook/`. This is offered, but never pre-selected: the user must
   tick an explicit "Allow storing without a keyring (local data will not be
   protected)" confirmation before it can be chosen (owner decision, 2026-09-25).
   Settings then show a persistent "Local data is not encrypted" notice with a
   one-click switch to option 1.

A hint is shown in both cases: installing `oo7-daemon` or `gnome-keyring` enables
the no-prompt mode. If a secret service appears later, Brook offers to move the
key there.

## 6. Rejected alternatives

- **Key derived from the account password:** a password change would force a
  re-encrypt on this device and leave every other device with data it can no
  longer open until the user types the new password there. Local data is
  disposable, so this coupling buys nothing.
- **A key compiled into the app ("only the app knows how to read it"):** anyone
  can extract it from the binary; it is obfuscation, not encryption.
- **Legacy macOS keychain:** it is the one with the "allow access" prompts the
  owner rightly distrusts.

## 7. Later, not in this spec

- Optional app lock (PIN / Touch ID), with the key in the Secure Enclave on
  Apple. It shares the machinery of §5 option 1.
- End-to-end encryption of messages is a different problem (keys shared across
  devices) and is out of scope here.

## 8. Owner decisions (2026-09-25)

1. **Sign-out** shows a "Remove this device's data" checkbox, **ticked by default**.
   Ticked: the data key is deleted first (crypto-erase, §3.5), then the local store,
   and a new key is made at the next sign-in. Unticked: they are kept
   (the next sign-in of the same user reuses them; a *different* user signing in
   still gets a wiped store, since one device's cache never crosses accounts).
2. **No keyring:** "no protection" is allowed, but only after the explicit
   confirmation in §5 option 2. It is never the default and never silent.
