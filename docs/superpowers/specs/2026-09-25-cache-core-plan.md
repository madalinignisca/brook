# Offline cache and outbox in core — implementation plan (#61, #63)

> Status: **draft for review** (Heavy) · 2026-09-25 · Implements §3–§5, §7 and §8 of the
> approved spec [2026-09-25-offline-cache-design.md](2026-09-25-offline-cache-design.md) against
> the server's `/sync` (#91) and idempotent send (#71). Files (#65, #67, spec §6) get their own
> plan once C1 lands: they build on its store layer. Tests first, each seen failing under a
> named mutation.

## Principles (what every step keeps true)
- **One apply function.** `/sync` pages, live events, history pages and send acks all go through
  it (§4.1). Nothing writes a cached row any other way.
- **Nothing at rest in cleartext:** every byte under a store directory is SQLCipher ciphertext,
  including `-wal`/`-shm`.
- **An unreadable key never deletes anything.** Only a *missing* key or a format change rebuilds.
- **The outbox never loses a send silently,** and a later message never overtakes an earlier
  one in the same channel.
- **The cache is on only with a durable `KeySlot`.** Until the Mac's keychain works (#79, the
  same switch as #92) the app is online-only, as today. `InMemoryKeySlot` would give a cache
  that's rebuilt at every launch and is never shipped.

## C0 — Spike: SQLCipher on both platforms (go/no-go, half a day)
- `rusqlite` with `bundled-sqlcipher`:
  - Apple: CommonCrypto (`SQLCIPHER_CRYPTO_CC`, the Security framework), arm64 only;
  - Linux: the system `libcrypto.so.3` (not vendored). The Linux client builds it on Arch.
- A throwaway test proves: a keyed open, WAL mode, a wrong key refused, and a sentinel string
  absent from the `.db`, `-wal` and `-shm` bytes.
- `cargo deny` accepts the new licences (Apache-2.0 for OpenSSL 3), and the xcframework size
  grows by a stated amount.
- **No-go:** stop and take the spec's alternative back to review. Don't improvise per-row
  encryption.

## C1 — Stores (spec §3)
- `store.rs`:
  - `Store::open(dir, slot_name, KeyStore)`: `get_or_create` the 256-bit key (P1's `KeyStore`),
    then `PRAGMA key` (raw hex key form), `cipher_memory_security = ON`, `temp_store = MEMORY`
    and `journal_mode = WAL`;
  - check `meta.format`. A mismatch in **cache.db** rebuilds it; in **outbox.db** it is
    surfaced (C4) before any rebuild;
  - **Missing vs unreadable, on evidence.** `KeyStore::get_or_create` (#92) gains an outcome:
    `KeyOutcome::{Loaded, Created}`. A `create` that finds `Exists` (a racing creator)
    reloads and counts as `Loaded`. The decisions:
    - `Loaded` and the database opens → normal;
    - `Created` and a database file already exists → its key is **missing** (proven absent,
      not merely failing): journal the old files, then rebuild. For **outbox.db**, first say
      "Unsent messages on this device couldn't be recovered" (the count is unknowable
      without the key);
    - `Loaded` but the database won't decrypt (a wrong key, or damage) → `StoreState::Damaged`:
      online-only, reported, and **nothing deleted**. A later explicit reset (sign-out with
      "Remove this device's data") is the only way out;
    - `Unavailable` / `Fatal` → `StoreState::Locked`: nothing opened, nothing deleted.
  - **Backups:** the Mac excludes the stores directory (`isExcludedFromBackup`). Linux:
    SQLCipher links the system `libcrypto.so.3`, so the Linux CI and release builds need the
    OpenSSL development package and INSTALL gets a line. That's the Linux client's call,
    raised with it in C0.
- **Store index:** `(origin, user id) → random store id`, in its own small SQLCipher database
  keyed by the `index` slot. Paths hold only the store id.
- Schema v1 exactly as spec §3 (cache.db, outbox.db). `rusqlite` runs on a dedicated blocking
  thread per store (one owner of the `Connection`, commands over a channel); no SQLite call
  on an async worker. The thread **stops cooperatively**: closing its channel ends its loop
  and a oneshot reports it stopped (a started blocking task can't be aborted, and C5's wipe
  must join it).
- **Tests:**
  - a sentinel is absent from all files after writes, mid-transaction and after a kill;
  - a store opens only with its own key;
  - a copied store file is unreadable once its slot is destroyed;
  - `Unavailable` at open deletes nothing and reports `Locked`.

## C2 — Apply and sync (spec §4)
- `apply.rs`, one function over a transaction: `apply(tx, Batch)`, where a `Batch` is rows
  from any source, tagged with the source:
  - the per-row `seq` guard;
  - the removal fence (`removed`), cleared only by **the caller's own** membership row with a
    higher `seq`. Within a batch, the caller's own membership rows are applied **first**, so a
    rejoin page's supplemental channel and member rows (sent whatever their `seq`, #91) land
    after the fence is cleared, in the same transaction;
  - history rows at `seq = 0`, applied only to a present, unfenced channel;
  - **membership rows are never physically deleted by sync:** `left_members` marks the row
    `left` with its `seq`. A later row for that pair applies only above it, so a delayed
    older membership can't resurrect a departure.
- **Mapping from #91's page:**
  - `channels`, `memberships`, `users`, `messages`: upsert through the guard;
  - `removed_channels`: fence plus a scoped removal (C5);
  - `left_members`: guarded mark as `left` (above).
  - `ChannelOut.unread_count` is ignored (always 0 in `/sync`). Unread is computed from the
    caller's `last_read_message_id` and the cached messages.
- **Sync loop:** after every (re)connect, `GET /sync?since=<cursor>` until `more` is false,
  then every few minutes. Each page and the new cursor commit in **one** cache.db transaction.
  `410 sync.reset` → rebuild cache.db (C5) → `since=0` (state only). Live WebSocket events
  (`message.new/update/delete`, `channel.delete` with `seq`) go through `apply` as they arrive.
- **Coverage (§4.3):** `coverage` rows; opening a channel with none fetches the head page;
  `before = oldest_id` extends down; a short page sets `complete_to_start`.
- **Tests:** every ordering and coverage test in spec §9, plus: a page and its cursor commit
  together (a kill between the page and the cursor leaves the old cursor and replays
  idempotently); `410` rebuilds cache.db and leaves outbox.db untouched.

## C3 — Read API and events (spec §8, reading half)
- `cached_channels()`, `cached_messages(channel, before?, limit)`, `pending_messages(channel)`
  and `cache_state()` (a watch receiver), plus a `CacheEvent { channel_id }` stream. FFI over
  UniFFI; the Mac UI is #62.
- **Tests:** offline launch returns the last-synced state; events fire after commit, never
  before.

## C4 — Outbox (#63, spec §5)
- `send_message(channel, body)` commits the row (`client_id` UUIDv4, a monotonic `ordinal`)
  **before** it returns. Attachments wait for the files plan.
- **Sender:** one task per channel, strictly by `ordinal`; a failed row blocks only its own
  channel. At startup, `sending` rows go back to `pending`.
- **Ack order:** (1) apply the returned message to cache.db through the guard and commit;
  (2) delete the outbox row matching the **echoed** `client_id`. A mismatched echo fails the
  row and it is never resent.
- **Status handling** (server contract, #81/#90/#71):

  | Answer | Row |
  |---|---|
  | network error, timeout, 5xx | pending; retry with backoff |
  | `408 file.upload_stalled`, `409 file.upload_in_progress`, `429 rate_limited` | pending; retry after `Retry-After` (upload PUTs restart from 0) |
  | `401` | refresh, then retry once. A refused refresh signs out: the sender **pauses** (no retries while signed out) and the rows wait for the next sign-in, or are surfaced by a wipe |
  | `507 file.no_space` (uploads, files plan) | **failed**, visibly, with Retry |
  | any other 4xx | **failed** with the server's code; Retry and Delete |

- Retry and Delete are serialised with the sender through the channel's task. Delete of an
  in-flight row waits for that attempt; if the send was accepted, it shows as sent.
- **Outbox unusable (§5.7):** unreadable → sending disabled with the stated message;
  unwritable → direct send only in a channel with no pending or failed rows.
- **Tests:** the spec §9 outbox list (minus the attachment items), plus the transient table
  row by row (each code: pending, not failed), and the paused sender while signed out.
- **Live (itest, spec §9):** offline read, a queued message, reconnect, exactly one send; an
  edit made elsewhere while offline appears after reconnect.

## C5 — Wipes and reconciliation (spec §7)
- A `generation` per store, in memory and in `meta`, checked by every writer before writing.
- **Store wipe:** bump the generation → cancel and join that store's tasks → close handles →
  destroy the key slot → delete files → report done. **Sign-out with "Remove this device's
  data"** runs this locally **first**, before the logout request, and completes offline. It
  extends #92's `logout` to `logout(remove_data)` and adds `unsent_count()`.
- **User switch** (#46 §8): other users' stores are wiped after their unsent count is surfaced.
- **Channel removal:** scoped, per spec §7.2 (fence, rows and journal in one transaction;
  cancel that channel's work; fail its outbox rows).
- **Deletion journal and startup reconciliation**, only after a store opened and read cleanly.
- **Tests:** every wipe row in spec §9 that doesn't involve files. Those move to the files plan.

## Where this fails
| Point | Failure | Response |
|---|---|---|
| C0 | SQLCipher won't build with CommonCrypto for arm64 Apple, or the size cost is unacceptable | stop; back to spec review |
| C1 | a keychain prompt or lock at open | `Locked`: online-only, nothing deleted (#92's `Unavailable`) |
| C2 | the server bypasses stamping (a bulk UPDATE, #91's stated limit) | that change is missed until the next edit of the row; raised as a server test obligation |
| C2 | a huge single-seq page (a channel with thousands of members deleted) | pages never split a seq, so one page is large; bounded by channel size, accepted |
| C4 | a crash between ack step 1 and 2 | resend, the server's 200 with the stored message, idempotent apply, then delete (tested) |
| C5 | a wipe racing a sync page | the generation check refuses the page's commit |

## If it stops halfway
- **C0 or C1 alone** is inert: nothing calls the stores yet.
- **C2 without C3:** the sync runs, but nothing reads the cache. It's behind the same
  durable-slot switch, so the shipped apps are unchanged.
- **C4 without C5:** sends queue and go out, but sign-out doesn't yet wipe. Not shippable:
  the durable-slot switch stays off until C5 lands, so no data reaches disk before it can be
  wiped.
- Each phase is its own PR, with the owning gate reviewed per phase.
- The durable-slot switch is **one function** that every store-opening path goes through, so
  no client, even one that already has a durable `KeySlot`, opens a store before C5.

## Review log
**Round 1: Codex and Vibe (Heavy).** (Vibe answered only once the brief was self-contained.)
- Accepted (Codex):
  - a missing outbox key is surfaced before the rebuild;
  - the backup exclusion and Linux build deps;
  - the live itests;
  - membership departures keep their `seq` (no physical delete);
  - the caller's own membership is applied first and is the only thing that clears a fence;
  - the cooperative worker stop;
  - `KeyOutcome`, so "missing" is proven, never inferred from a decrypt failure;
  - the switch as one function.
- Accepted (Vibe):
  - the sender pauses while signed out;
  - `507` is its own row.
- Rejected (Vibe), with reasons: "a rejoin lets old messages reappear". After a rejoin the
  channel's history is legitimately visible again on the server. Messages edited or deleted
  meanwhile carry higher `seq`s, so the per-row guard keeps their newest state; the fence
  exists to stop resurrection *before* a rejoin, which it still does.
