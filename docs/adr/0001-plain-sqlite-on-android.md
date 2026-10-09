# ADR 0001: Plain SQLite on Android

Date: 2026-10-08

## Status

Accepted

## Context

The local offline cache and outbox stores (spec 2026-09-25-offline-cache-design.md §3) are built as **SQLCipher** databases on desktop and iOS platforms. SQLCipher provides at-rest encryption with a per-store random key held in the platform's secure key store (`KeySlot`).

However, on Android:
- SQLCipher depends on **OpenSSL / libcrypto**, which the NDK does not provide.
- Android's sandbox model grants each app exclusive access to its private `app_private` directory (Linux user isolation + SELinux).
- The OS encrypts all app private storage via **File-Based Encryption (FBE)** for free, transparent to the app.

Given this, adding a vendored OpenSSL dependency to the Android build is unjustified overhead when the same protection — ciphertext at rest, kept from other apps — is already provided by the OS.

## Decision

**Local stores on Android use plain SQLite, not SQLCipher.** The decision is gated on `target_os = "android"` at compile time: only Android builds use plain SQLite; all other platforms (Linux, macOS, iOS, Windows) use SQLCipher with a random key.

**Why desktops keep SQLCipher:** On Linux and Windows, the app's data directory is not protected for the app's use alone. A second user or a physical attacker with disk access could read the database files. SQLCipher protects against this. macOS' `~/Library/Application Support/` is user-writable by default (before recent SIP hardening), so SQLCipher is also necessary there.

**Why iOS decided differently (#272):** iOS apps run in a sandbox with mandatory code signing and secure enclave hardware; the OS guarantees file protection, and relying on it avoids the OpenSSL dependency on mobile.

## Consequences (confirmed by owner, spec §4)

- **Protection mechanism**: on Android, the sandbox plus FBE provide the encryption barrier. An app reading another app's files is impossible. An attacker with the device would need to decrypt the partition (OS task), not the app.
- **No wrong-key detection**: unlike SQLCipher (which fails to open if the key doesn't match), plain SQLite opens any file. No "locked" or "missing key" state for Android stores. A readable but corrupt database signals damage, not a key issue.
- **File keys in plaintext**: per-file encryption keys (for attachments, in the `files` table) are stored plaintext inside the database. They are as protected as the database itself: under FBE. This is acceptable because the file's encryption is not the sole protection — the file's ciphertext is also guarded by the app's sandbox.
- **Sign-out wipe is file deletion**: crypto-erase (secure deletion) is a server responsibility, not the app's. A store wipe on Android deletes the files and directories; deleted files may recover from filesystem free blocks (as on any OS).
- **No `Locked` state**: the `core` API has no "waiting for the key to unlock" state on Android. The store either opens cleanly, or is marked damaged and rebuilt.

## Requirements

- Credential-encrypted storage: tokens and session keys continue to live in the platform's secure key store, not on the filesystem (Android `EncryptedSharedPreferences` or equivalent). [Deferred to separate work; #46 §8a already specifies this.]
- Every backup must **exclude** the `stores/` directory (app-private offline cache is not backed up). On Android this is the default (private app storage is never backed up).
