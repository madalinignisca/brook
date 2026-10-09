# ADR 0001: Plain SQLite on Android

Date: 2026-10-08

## Status

Accepted (2026-10-08, owner; #273)

## Context

The local offline cache and outbox stores are built as **SQLCipher** databases on every platform except Android, using at-rest encryption with a per-store random key in the platform's secure key store.

On Android:
- Use what the device does better: each app's private storage has its own UID sandbox and is encrypted at rest via **File-Based Encryption (FBE)**, mandatory on every Android 13 device.
- SQLCipher depends on **libcrypto** for the target, which the NDK does not provide.

## Decision

**Local stores on Android use plain SQLite, not SQLCipher.** The change is gated on `target_os = "android"` at compile time: only Android builds use plain SQLite; all other platforms use SQLCipher.

Desktops keep SQLCipher because they have no per-app sandbox.

iOS keeps SQLCipher today; it is decided in #272.

## Consequences

Protection on Android is the app sandbox plus file-based encryption, nothing more.

The store's wrong-key detection does not apply.

File sealing adds nothing there: each downloaded file's own key is kept in a `key BLOB` column of the store, and each outgoing file's snapshot key (`outbox_files.key` in `outbox.db`) is kept the same way. On Android, both sit in plain SQLite next to the files they seal.

Sign-out wipe is not a crypto-erase. A store wipe on Android relies on deleting the files, not on destroying a key first.

A store never reports `Locked` because of a key it does not use.

A damaged plain store is reported `Damaged` and kept, never rebuilt.

## Requirements

- The databases live in credential-encrypted storage, never device-protected storage.
- They stay out of every backup: in the no-backup directory, plus `allowBackup` and `dataExtractionRules` keeping them out of cloud backup and device-to-device transfer.
