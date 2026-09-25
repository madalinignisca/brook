# KeySlot and staying signed in — implementation plan (#59, #58)

> Status: **draft, review round 1 applied** · 2026-09-25 · Dial: **Heavy** (secrets at rest).
> Implements the local-encryption spec #46 (§3.4–§3.6, §3a, §4, §8) for core and Apple; the
> Linux backend (`oo7`) is the Linux client's. Tests first, each seen failing under a named
> mutation.

## Principles (what every step below must keep true)
- **A revoked token can't restore a session.** Restore always goes through `/auth/refresh`,
  so a stored token the server has revoked restores nothing; it only costs a prompt to sign in.
  The dangerous case is a stored token that's still **valid** after the user signed out. Every
  sign-out path therefore (a) revokes on the server best-effort and (b) makes the stored copy
  unusable locally, with a durable fence if deletion fails.
- **Persistence mirrors the store, in the store's order.** Every slot write carries the store
  `Revision` (epoch and credential revision) it mirrors. A single **persister** applies them in
  revision order and drops anything older than what it last applied. So a slow write can never
  undo a newer one or a sign-out.
- **Quit isn't sign-out.** Dropping the client when persistence is on keeps the stored session
  (and doesn't revoke it). Only explicit sign-out and remote sign-out clear it.
- **Off until complete.** Persistence is enabled only by an explicit `with_key_slot`, once every
  lifecycle path below is wired. A build with persistence disabled **deletes** any stored
  session at launch (the rollback path), so an older or partial build never restores a stale
  one.

### P1 — Core: `KeySlot` and the key store
- The UniFFI foreign trait, **synchronous**:
  - `load(slot)` → `Option<bytes>`;
  - `create(slot, bytes)`: create-only, returns `Exists` if taken;
  - `replace(slot, bytes)`: an **atomic** overwrite-or-create (`SecItemUpdate`, or add if
    absent; oo7 `create_item(replace: true)`);
  - `delete(slot)`.
  Errors: `Exists`, `Unavailable` (locked, `errSecInteractionNotAllowed`, `errSecAuthFailed`,
  a locked Secret Service), and `Fatal(code)`. `Fatal` carries a **numeric status code
  only**, never backend text.
- `KeyStore::get_or_create(slot)`: load; if absent, `getrandom` 32 bytes (an entropy failure is
  an error, never a weaker key), then `create`. On `Exists`, load again; if that load is absent
  or unreadable, return `Unavailable`, never a new key.
- Key and token bytes are held in `Zeroizing`. Every type carrying them has a redacted `Debug`.
- `InMemoryKeySlot` for tests, with scripted failures per call.
- **Check, each with a mutation:**
  - absent → created once, 32 random bytes;
  - `Exists` → the winner's key; `Exists` then an absent load → `Unavailable`;
  - `Unavailable` and `Fatal` never create, delete or replace;
  - slots are isolated (one slot's operations never touch another's);
  - an entropy failure makes no key;
  - no key bytes in `Debug` or logs (canaries).

### P2 — Core: staying signed in (#58)
- **Stored:** slot `session:<origin>` holds `{saved_rev, user id, handle, refresh token}`
  (redacted `Debug`). The access token is never stored.
- **The persister:** one task per client, fed `(Revision, Write | Clear)` from inside the store
  write that made the change:
  - install (login, TOTP), commit (refresh, password change, activation) → `Write` with the new
    pair's revision;
  - `logout` and a matching `clear_if_holds` → `Clear`. It clears only if the clear matched the
    session, so a stale rejection can't clear a newer user's session.
  The persister applies them in revision order with `replace`, never delete-then-store, and
  skips anything older than its last applied revision.
- **When a write fails:**
  - a `Write` failing with `Unavailable` or `Fatal` leaves the older stored token, which is now
    stale. The persister marks the slot **stale**: it records the durable fence (below) and
    retries `replace` on the next change or launch;
  - a stale stored token at restore gets 401, and the slot is cleared;
  - a failed `Clear` records the fence.
- **Durable sign-out fence:** a small non-secret file in the app's data directory,
  `signed-out/<hash(origin)>`, holding the revision of the sign-out. It's written whenever a
  `Clear` is issued, **before** the keychain delete is attempted. Restore refuses any stored
  session whose `saved_rev` is at or below the fence, and deletes it. Plain file writes
  succeed when the keychain is locked, so a sign-out is honoured even if its delete failed.
- **Restore** (`BrookClient::restore()`, only when no session is live):
  1. it is a login attempt: `reserve_login` generation, then the refresh lock;
  2. check the fence; load the slot (`Unavailable`/`Fatal` → `RestoreOutcome::Unavailable`,
     nothing deleted; absent → `NotSignedIn`);
  3. refresh with the stored token. The new pair is **persisted at once** (a `Write`),
     before `/me`, so a rotated credential is never left unpersisted;
  4. `/me`, and it must be the stored user id (a mismatch clears the slot and returns
     `NotSignedIn`);
  5. `install_for_login(generation)`: a logout or a newer login meanwhile wins, and the pair is
     revoked.
  Outcomes:
  - a rejected refresh → `Clear`, `NotSignedIn`;
  - a network error → `Offline`, slot kept;
  - `/me` failing after rotation → `Offline` with the new pair kept, retried.
- **Drop with persistence on:** the store is fenced as today, but the session is **not**
  revoked and the slot is untouched. Without persistence, Drop revokes as in #74.
- **Server contract (raised with the server):** a replayed or revoked refresh token only
  affects its own **login lineage**. Today the server only refuses it. The planned reuse
  detection (`auth.py` TODO) must revoke that one family, never the user's other sessions. So a
  stale stored token can at worst sign out the device that stored it, which is what already
  happens.
- **Check, each with a mutation:**
  - restore after login, refresh, password change and TOTP activation (the stored token is the
    latest);
  - no restore after logout or a remote sign-out;
  - a stale rejection after a newer commit doesn't clear;
  - a failed `Clear` is honoured by the fence;
  - a failed `Write` leaves a stale slot that restores to `NotSignedIn`, never a valid old
    session;
  - an interrupted rotation (the server rotated, the client died before its write) →
    `NotSignedIn`;
  - restore racing logout, a newer login, close and Drop: never installed;
  - `/me` failure after rotation keeps the new pair;
  - a user mismatch clears;
  - disabling persistence deletes a stored session at launch;
  - Drop keeps the stored session and doesn't revoke it;
  - canaries: the refresh token appears in no log, no `Debug`, and no error.
  Mutations: delete-then-store instead of `replace`; the persister ignoring revision order; a
  `Clear` on an unmatched rejection; no fence; persisting after `/me`; Drop revoking.

### P3 — Apple: `KeychainSlot` (Swift)
- `SecItemAdd`, `SecItemCopyMatching`, `SecItemUpdate`, `SecItemDelete` with
  `kSecUseDataProtectionKeychain`:
  - `kSecClassGenericPassword`, service `dev.brook.Brook.datakey`, `kSecAttrAccount` = slot;
  - `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`, `kSecAttrSynchronizable: false`;
  - access group `$(AppIdentifierPrefix)dev.brook.shared`, expanded by Xcode from the
    provisioned App ID prefix. The built entitlement is checked with `codesign -d
    --entitlements`.
- Error map:
  - `errSecItemNotFound` → absent;
  - `errSecDuplicateItem` → `Exists`;
  - `errSecInteractionNotAllowed` and `errSecAuthFailed` → `Unavailable`;
  - anything else → `Fatal(status)`.
- **Single instance:** a `flock` on a file in the app's data directory. A second instance runs
  with persistence off (`InMemoryKeySlot`) and says so. So two processes never rotate the same
  stored token.
- **Owner step (#79):**
  - an App ID `dev.brook.Brook` with the keychain group;
  - a **development** profile (Debug) and a **Developer ID** profile (Release), selected by
    `PROVISIONING_PROFILE_SPECIFIER` in the gitignored `Local.xcconfig` and embedded by Xcode.
  Until then, the app runs with persistence off.
- **Check:**
  - the error map against a fake `SecItem` layer (the unsigned test host can't reach the
    data-protection keychain);
  - the entitlement verified on the built Release;
  - a manual run of the signed build: sign in, quit, relaunch and be signed in; Sign Out,
    relaunch and see sign-in; sign out with the device locked is still honoured (the fence).

### P4 — macOS app
- On launch, the single-instance check, then `restore()`, showing "Signing in…". Then
  signed-in or the login screen, with a note only when useful.
- The restore is an attempt, so #74's fencing covers it.
- Sign Out and a remote sign-out clear through core.
- **Check:** model tests per `RestoreOutcome`; a remote sign-out → the next launch is signed out;
  a quit → the next launch is signed in.

## Where this fails
| Point | Failure | Response |
|---|---|---|
| P2 | the keychain is locked when a rotation must be written | stale slot + fence; the next launch signs in by hand, and never restores an old valid session |
| P2 | a crash between the server's rotation and the write | the stored token is dead → `NotSignedIn`; the server contract limits reuse detection to that lineage |
| P3 | no profiles yet | persistence off; only #79 blocks |
| P3 | a second instance | persistence off for it |

## If it stops halfway
- P1 is additive.
- P2 is inert without `with_key_slot`, and a build without persistence **deletes** any stored
  session at launch, so a rollback never restores stale credentials.
- P3 without profiles is dormant.
- P4 turns persistence on only once P2's lifecycle tests pass.

## Review log
**Round 1 — Codex + Vibe (Heavy).** Accepted:
- an atomic `replace`;
- a revision-ordered persister, and clearing only on a matched rejection;
- a durable sign-out fence (a plain file) for failed deletes;
- stale-slot handling;
- restore as a fenced login attempt, persisting before `/me` and checking the user id;
- a single-instance lock;
- quit isn't sign-out (Drop keeps and doesn't revoke when persistence is on);
- redacted `Debug` and numeric-only `Fatal`, with token canaries;
- `AppIdentifierPrefix`, dev and Developer ID profiles, and entitlement verification;
- persistence gated off until complete, and a rollback that deletes;
- `errSecAuthFailed` → `Unavailable` (Vibe);
- the full failure-test list.
Rejected with reasons: that a stale token's replay can revoke valid sessions elsewhere. The
server has no reuse detection today (the `auth.py` TODO), and the planned one is per login
family, so only the storing device's own lineage is affected. That's stated above as a server
contract and raised with the server.
