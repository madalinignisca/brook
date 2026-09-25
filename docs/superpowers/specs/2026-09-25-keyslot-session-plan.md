# KeySlot and staying signed in — implementation plan (#59, #58)

> Status: **draft for review** · 2026-09-25 · Dial: **Heavy** (secrets at rest). Implements the
> local-encryption spec #46 (§3.4–§3.6, §3a, §4, §8) for core and Apple; the Linux backend
> (`oo7`) is the Linux client's. Tests first, each seen failing under a named mutation.

### P1 — Core: `KeySlot` and the key store
- The UniFFI foreign trait `KeySlot { load(slot), store(slot, key), delete(slot) }`, which is
  **synchronous**. Errors: `Exists`, `Unavailable`, `Fatal(String)`.
- `KeyStore` over it:
  - `get_or_create(slot)`: load; if absent, generate with `getrandom` and store
    **create-only**; on `Exists`, load again (a race lost to another process);
  - `destroy(slot)`.
  Key bytes are wrapped in `Zeroizing` and never logged or shown in `Debug`.
- `InMemoryKeySlot` for tests (and for any build with no secure store), with scripted
  failures.
- **Check:**
  - absent → created once; `Exists` → the winner's key is used;
  - `Unavailable` and `Fatal` never create, delete or overwrite;
  - `destroy` removes;
  - no key bytes in `Debug` or logs.
  Mutations: overwrite on `store`; creating on `Unavailable`; treating `Fatal` as absent.

### P2 — Core: staying signed in (#58)
- `BrookClient::with_key_slot(slot: Arc<dyn KeySlot>)` enables persistence. Without it,
  nothing changes (tests, and the Linux client until its backend lands).
- **What is stored:** slot `session:<origin>` holds `{user id, handle, refresh token}` as
  serialized bytes. The access token is never stored (it's short-lived and re-obtained).
- **When:** in the **same critical section as the store change it mirrors**:
  - install (login, TOTP completion) writes it;
  - every committed refresh, password-change or activation commit overwrites it (`delete` +
    `store`, under the refresh lock), because rotation makes the old token dead;
  - `logout`, `clear_if_holds` (a remote sign-out) and `close` delete it;
  - a different user signing in replaces it.
- **Restore:** `BrookClient::restore()`:
  - load the slot;
  - `Unavailable` or `Fatal` → `RestoreOutcome::Unavailable` (sign in by hand; nothing
    deleted);
  - absent → `NotSignedIn`;
  - else install a session from the stored refresh token by refreshing (the refresh-lock path),
    then `/me`. `Committed` → `LoggedIn`; a rejected refresh → the slot is deleted and
    `NotSignedIn`; a network error → `Offline` (the slot is kept, retried later).
- **Crash window, stated:** if the app dies between the server rotating a token and core
  writing the new one, the stored token is dead. The next launch's refresh is rejected, the
  slot is deleted, and the user signs in again. No sign-in is ever resurrected wrongly; this is
  the pre-1.0 cost.
- **Check:**
  - restore after login, after a refresh (the rotated token is the one stored), and after a
    password change or TOTP activation;
  - no restore after logout or a remote sign-out;
  - a rejected stored token deletes the slot;
  - an unavailable slot deletes nothing;
  - a different user replaces it;
  - the stored bytes never contain the access token.
  Mutations: persisting outside the lock (a race test: a refresh committing while logout
  runs must not leave a stored session); not overwriting on refresh; not deleting on a
  remote sign-out.

### P3 — Apple: `KeychainSlot` (Swift)
- A `KeySlot` implementation:
  - `SecItemAdd`, `SecItemCopyMatching`, `SecItemDelete`, with `kSecUseDataProtectionKeychain`;
  - `kSecClassGenericPassword`, service `dev.brook.Brook.datakey` (slots in
    `kSecAttrAccount`);
  - `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`;
  - `kSecAttrSynchronizable: false` on add and query;
  - access group `$(TeamID).dev.brook.shared`.
- Error map:
  - `errSecItemNotFound` → absent;
  - `errSecDuplicateItem` → `Exists`;
  - `errSecInteractionNotAllowed` → `Unavailable`;
  - `errSecMissingEntitlement` and the rest → `Fatal`.
- **Needs the owner (Apple Developer account):** an App ID for `dev.brook.Brook` with the
  keychain access group, and a **Developer ID provisioning profile** for it. The profile's
  path goes in the gitignored `Local.xcconfig`, and the entitlement goes in the shipped
  entitlements.
  - Until then, the app uses `InMemoryKeySlot`: persistence is off, and it says "Brook will ask
    you to sign in each time" in Settings.
  - Nothing else is blocked.
- **Check:**
  - the error map, unit-tested against a fake `SecItem` layer (the unsigned test host can't
    reach the data-protection keychain);
  - a manual run of the signed Release: sign in, quit, relaunch and be signed in; Sign Out,
    relaunch and see the sign-in screen.

### P4 — macOS app
- On launch, `SessionStore` calls `restore()` for the last server: a brief "Signing in…",
  then signed-in or the login screen, with a note only when useful ("Couldn't reach the
  server; showing sign-in"). Sign Out deletes the slot (core does).
- **Check:**
  - model tests with a fake client for each `RestoreOutcome`;
  - a remote sign-out deletes the slot, so the next launch starts signed out;
  - the attempt fencing from #74 still holds with a restore attempt.

## Where this fails
| Point | Failure | Response |
|---|---|---|
| P2 | the persist write fails (keychain full or locked) | the session stays in memory, and the next launch signs in by hand; logged by kind |
| P3 | no provisioning profile yet | `InMemoryKeySlot`; the owner step is the only blocker, and it's stated |
| P2 | two app instances | the create-only `store` plus reload; the last writer wins on overwrite, and both hold valid sessions |

## If it stops halfway
P1 is additive. P2 changes nothing unless `with_key_slot` is used. P3 without the profile is
dormant. P4 is the user-visible part and needs P3's profile to be useful. The GTK client adopts
P2 when its `oo7` backend lands.
