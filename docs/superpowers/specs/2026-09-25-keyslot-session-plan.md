# KeySlot and staying signed in — implementation plan (#59, #58)

> Status: **approved** (Heavy, two rounds; the implementation gate re-examines the simplified design) · 2026-09-25 · Dial: **Heavy** (secrets at rest).
> Implements the local-encryption spec #46 (§3.4–§3.6, §3a, §4, §8) for core and Apple; the
> Linux backend (`oo7`) is the Linux client's. Tests first, each seen failing under a named
> mutation.

## Principles (what every step below must keep true)
- **A revoked token can't restore a session.** Restore always goes through `/auth/refresh`, so
  the dangerous case is only a stored token that's still **valid** after a sign-out.
- **Write-through, in the store's own critical section.** Keychain calls are synchronous and
  fast. Every persistence operation runs **inside the session store's write section**, next to
  the change it mirrors: install, commit, sign-out or clear. So the store's order *is* the
  persistence order. There's no background persister, no queue and no revision numbers.
- **Quit isn't sign-out.** With persistence on, Drop fences the store but neither clears the
  slot nor revokes the session.

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
- **Stored:** slot `session:<origin>` holds `{user, refresh token}` (the full `User`, so a restore
  needs no `/me` before it can install), with a redacted `Debug`. The access token is never
  stored.
- **Writes, each inside the store write section, before it returns:**
  - install (login, TOTP completion) and every commit (refresh, password change, TOTP
    activation) do an atomic `replace`;
  - on success, a sign-out fence (below) for that origin is removed;
  - if `replace` fails, the **stale** copy is still there. It holds an older refresh token that
    this commit just rotated, so the server will refuse it; unless it belongs to a
    *different* user (a new login replacing another account), in which case core writes the
    fence to cover it.
- **Clears, each inside the store write section, before `logout` returns:**
  - `logout` and a matching `clear_if_holds` call `delete`;
  - if `delete` fails, the **fence** is written;
  - if writing the fence fails too, `logout` still completes locally and reports
    `SignOutIncomplete`, and the app says "Brook couldn't forget this sign-in on this Mac". The
    server-side revoke (best-effort, as in #74) is the remaining backstop.
- **The fence:** `signed-out/<hash(origin)>` in the app's data directory, written atomically
  (temp file, `fsync`, `rename`, `fsync` of the directory). Its presence means: don't restore;
  delete the stored session at the next chance. An unreadable fence directory counts as "fence
  present" (fail closed). Only a successful `replace` after a new sign-in removes it.
- **Restore** (`BrookClient::restore()`, only when no session is live), as a login attempt
  (`reserve_login` generation, then the refresh lock):
  1. fence present → delete the slot (best-effort) → `NotSignedIn`;
  2. load: absent → `NotSignedIn`; `Unavailable`/`Fatal` → `Unavailable` (nothing deleted);
  3. refresh with the stored token:
     - rejected → delete the slot **only if it still holds that exact token** (compared inside
       the store write section), then `NotSignedIn`;
     - a network error → `Offline`, slot untouched;
  4. accepted → in **one** store write section, if the generation is still current: `replace`
     the slot with the new pair, then install and publish. If a logout or a newer login won,
     nothing is written or installed, and the new pair is revoked.
  Because install and persist are one step, there's no un-installed candidate in limbo. `/me`
  runs afterwards as an ordinary refresh of the user's details.
- **Persistence modes, kept separate:**
  - **On:** a `KeySlot` that reaches the keychain.
  - **Unavailable:** no profile, an entitlement fault, or a second instance. Core gets
    `InMemoryKeySlot` and never touches the real keychain, so it can neither restore nor delete
    another instance's credentials.
  - **Rollback kill switch:** a build flag `forget_stored_session`, only in builds that
    *can* reach the keychain, deletes the stored session and writes the fence at launch. That's
    how a release turns persistence off safely.
- **Check, each with a named mutation:**
  - persist order equals store order under concurrent refresh and logout (mutation: persist
    after releasing the store lock);
  - sign-out deletes before `logout` returns (mutation: deferred delete);
  - a failed delete writes the fence, and restore honours it after a simulated relaunch
    (mutation: no fence);
  - fence write failure → `SignOutIncomplete` (mutation: silent);
  - an unreadable fence directory refuses restore (mutation: fail open);
  - a new sign-in removes the fence only after its own write succeeded (mutation: remove first);
  - a failed `replace` on a user switch fences the old user's still-valid token; relaunch →
    `NotSignedIn`, not the old user (mutation: no fence on a user switch);
  - a rejected restore deletes only its own token when a newer login raced it (mutation:
    unconditional delete);
  - a restore losing to logout writes nothing: relaunch → `NotSignedIn` (mutation: write
    before the generation check);
  - Drop keeps the slot and doesn't revoke (mutation: Drop revokes);
  - a second instance never touches the slot (mutation: instance 2 using the keychain);
  - the kill switch deletes and fences;
  - canaries: the refresh token appears in no log, no `Debug`, and no error.
- **Server contract (raised with the server):** a replayed or revoked refresh token only
  affects its own login lineage; the future reuse detection must enforce and test that.

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
- **Single instance:** a `flock` on a file in the app's data directory, taken before `restore` and
  **held for the life of the process**. A second instance runs in the *unavailable* mode
  (`InMemoryKeySlot`), touches nothing, and says so.
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
| P2 | the keychain is locked when a rotation must be written | the stale copy holds a rotated (refused) token; the next launch signs in by hand |
| P2 | delete and fence both fail at sign-out | `SignOutIncomplete` shown to the user; the server revoke is the backstop |
| P2 | a crash between the server's rotation and the write | the stored token is dead → `NotSignedIn`; the server contract limits reuse detection to that lineage |
| P3 | no profiles yet | persistence off; only #79 blocks |
| P3 | a second instance | persistence off for it |

## If it stops halfway
- P1 is additive.
- P2 is inert without `with_key_slot`. Rollback is the explicit kill switch (delete + fence), not
  "persistence off", so a second instance or a build without keychain access never deletes
  another instance's credentials.
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

**Round 2 — Codex + Vibe.** Vibe: none. Codex no longer objects to the rejected point (it's
covered by the server contract) and raised seven new points about the asynchronous persister:
- fence durability;
- revisions across launches;
- restore's early write;
- conditional cleanup;
- unacknowledged writes;
- conflicting persistence-off modes;
- mutation coverage.
All are accepted, answered by **simplifying**: synchronous write-through inside the store's
write section (so there's no persister and no revisions); install and persist as one step on
restore (no candidate in limbo); an atomically written, fail-closed fence that a later
successful sign-in removes; deletion conditional on the exact token; three separate
persistence modes; the single-instance lock held for the life of the process; and one named
mutation per check. No round 3 by rule. The simplification is new design, so the
implementation gate reviews it in full.

**Implementation gate (Heavy).** Codex reproduced every finding with a probe. Vibe reviewed the
Apple half. Its two points were rebutted with evidence: `kSecUseDataProtectionKeychain` in a
query returns `errSecItemNotFound` for a missing item, and the restore is bounded by core's 30 s
request timeout. On the core half it produced no output after four attempts, so that half has
one reviewer.
- Round 1 accepted:
  - clients in one process share the slot;
  - a quit mid-refresh must store the rotated token, not revoke it;
  - the Mac warning must not depend on form state;
  - re-assert `ThisDeviceOnly` on update;
  - fsync the fence's parent;
  - a pre-existing refresh-error body leak.
- Round 2 found holes in the first fix (per-attempt tickets). The design changed to: **the
  newest client to enable persistence owns the slot**. Every slot operation, reads included,
  checks that under one process-wide lock. The app makes a new client per attempt, so the owner
  is always the current attempt. A rotation is followed after a quit only while the session is
  still persisted, never after a sign-out. No round 3 by rule.

**Server review.** `/auth/refresh` answers only 200, 401, 422 or 429. So only 401 and 422
mean the token was refused (deleted if it's still the stored one). Any other 4xx comes from
something in front of the api and is transient, like a 5xx: Offline at restore, a retry in the
refresh loop.
