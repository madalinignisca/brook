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
   is missing or unreadable (reinstall, wiped keychain, restored backup on a new
   machine), the app deletes its local store, signs in again and re-syncs.
   Nothing is ever pulled from other devices.

## 4. Where the key lives, per platform (strongest first)

| Platform | Store | Isolation from other apps | User prompts |
|---|---|---|---|
| macOS / iOS | Data-protection Keychain (`kSecUseDataProtectionKeychain`), `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`, not synchronizable, the app's own access group | **Yes**, enforced by code signature | None |
| Linux, Flatpak | Secret portal (`org.freedesktop.portal.Secret`): a per-app secret handed only to this app ID; data key derived from it with HKDF | **Yes**, per app ID | None |
| Linux, native, with a Secret Service (gnome-keyring, KWallet) | Secret Service item tagged for Brook | **No**: any app in the session can read it | Usually none; it's unlocked at login |
| Linux, no secret service (bare sway, etc.) | see §5 | see §5 | see §5 |
| Windows (later) | DPAPI, user scope | No (user-scoped, not app-scoped) | None |

In Rust, the `oo7` crate speaks both the Secret portal (inside Flatpak) and the
Secret Service (outside), so core can own one Linux backend. Apple needs a small
Swift keychain shim behind a core callback.

**Flatpak is the Linux configuration that actually delivers app isolation.** The
native tarball (#38) works, but the settings screen says "Protected from: backups,
file browsing. Not from: other apps you run."

## 5. No secret service at all (bare sway)

The Secret portal and the Secret Service both need a backend running
(gnome-keyring, KWallet, or the standalone `oo7-daemon`). On a bare sway session
often none runs, and a Flatpak's portal then has nothing to hand out.

Brook detects this on first run and **asks once, never silently degrading**:

1. **Local passphrase (recommended here).** The user picks a passphrase used only
   on this device. It has nothing to do with the account password. The data key is
   wrapped with Argon2id(passphrase), and Brook asks for the passphrase when it
   starts: effectively an app lock. Forgetting it costs only the local copy (§3.4).
2. **No protection.** The data key sits in a 0600 file in
   `$XDG_DATA_HOME/brook/`. Settings show a persistent "Local data is not
   encrypted" notice with a one-click switch to option 1.

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

## 8. Open questions for the owner

1. On sign-out, should local data be wiped by default, or kept until the next
   sign-in as the same user? (Proposed: wiped when a *different* user signs in;
   kept otherwise.)
2. §5 first-run choice: acceptable, or should "local passphrase" be the only
   option on a machine with no secret service?
