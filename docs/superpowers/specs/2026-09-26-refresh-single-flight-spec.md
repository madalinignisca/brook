# One refresh at a time per stored session (#120)

Status: spec, closed after review round 2. Review dial: **Heavy** (auth: refresh-token rotation, family revocation).

## The failure

The app makes a new `BrookClient` for every sign-in attempt, and the newest client with
persistence owns the stored session (`persist.rs` `OWNERS`). An older client can still be
alive, with its refresh loop running, while the newer one restores from the stored token.
Its session is the same login (token family) whenever both came from the same stored token.

1. The old client's loop rotates T into T1. It no longer owns the slot, so T1 isn't stored.
2. The new client restores from the stored T. Within #104's grace, the server answers with
   T2 and retires T1.
3. The old client's next refresh presents T1, which is retired. The server reads that as
   reuse (theft) and ends the family, and with #116 that signs the new client out too.

Step 2's grace works only while T1 is unused. Had the old client refreshed T1 again after
losing the slot, the new client's restore of T would itself be the reuse verdict, which is
why a client without the slot must send nothing at all (1 and 3 below). This was confirmed
against the server's `_rotate` as deployed: a second use of T within the grace revokes the
successor S (with `replaced_by_id` cleared), so presenting S afterwards falls through to the
theft branch.

The reverse order breaks the same way: the new client's refresh lands first, then the old
client's in-flight refresh of T takes the grace and retires the new client's token.

## Where it happens in the shipped apps

GNOME uses one client. The Mac app restores only at launch, before any other client
exists, and a password sign-in starts a new family. The realistic trigger is a **revoke**,
not a refresh (review round 1):
- the previous attempt's client is dropped, or signed out locally, while its refresh is in
  flight;
- `commit_refresh` then discards the fresh pair: the client is closed, and it doesn't own
  the slot, so `follow_rotation` is a no-op;
- `refresh_once` revokes that pair, and since #116 a logout ends the whole family, the
  owner's included.

The same holds for three other revoke paths on a client without the slot: `logout()`, a
login that displaces its session, and a stale restore.

## Done means

1. **A client that isn't its slot's owner revokes nothing of the stored login.** The
   exemption keys on where the token came from, not only on who owns the slot now. A
   token is *from the slot* if it was restored from it, written to it while this client
   owned it (a login or challenge installed with persistence), or rotated from such a
   token. The session cell carries that as a flag, set at those installs and kept
   through `commit_refresh`. `revoke_detached` skips a token only if persistence is on,
   this client doesn't own the slot, **and** the token is from the slot. A fresh login's
   pair that never installed (`Stale`, `SlotTaken`, a refused challenge) is always
   revoked: it was never the owner's, and leaving it would keep it live for 7 days
   (round 2). The paths covered:
   - a refresh or a password call whose fresh pair is discarded;
   - `logout()`;
   - a login that displaces a session;
   - restore's `Stale` branch.
   An exempt token is dropped rather than revoked: it belongs to the owner's login, and
   revoking it would end that. A client without persistence revokes as before.
2. **A client that isn't the owner never refreshes.** `refresh_once`, the one function
   every refresh goes through, checks ownership (a read-only `owns()`, never holding
   `OWNERS` across the keychain) under the slot lock, right before the request. Every
   caller goes through it: the refresh loop, the pre-request refresh, the WebSocket's 1008
   recovery, and the password flows. A client that fails the check sends nothing and
   ends its session locally with `sign_out(false)` and **no revoke** (1). It clears
   nothing, since `with_slot(clear)` is a no-op for a non-owner and must not clear the
   owner's copy, and `sign_out_complete` stays true. Restore keeps its own check, where
   it reads the slot.
3. **Refreshes of one slot are serialized process-wide**, by an async lock per slot name.
   It's taken **right after `refresh_lock`**, where `refresh_lock` is taken (restore,
   `Refresher::refresh`, the account sections), never inside `refresh_once`: taking it
   there too would lock twice on the password flows' held-lock path. The order is always
   `refresh_lock`, then the slot lock, and nothing takes a `refresh_lock` while holding a
   slot lock, so there's no cycle across clients. A refresh in flight when ownership moves
   finishes before the new owner's restore reads the stored token.
4. **"Owner" means the newest client to enable persistence**, as today. The Mac app makes
   one per sign-in attempt and drops the previous one, and is never signed in on an older
   client while a newer one exists. A superseded client that is still some UI's current
   client would show "signed out", not an error. Neither app has one today, and on the
   Mac, `follow` already drops a superseded attempt's events.
5. Tests, each watched failing under a mutant:
   - **the realistic trigger:** an older client, signed in from the stored token, is
     dropped with its refresh held. A newer client restores. Released, the owner stays
     signed in and the test server records no logout of that family.
   - `logout()` on a superseded client revokes nothing, and the owner stays signed in.
   - a superseded client's refresh (loop, and WebSocket recovery) sends no request and
     ends its session locally.
   - two clients over one slot, with the older's refresh held while the newer claims and
     restores: no reuse is recorded.
   - a client without persistence refreshes and revokes as before.

## Not doing

- Across processes. On the Mac, a second instance can't persist (`SessionPersistence`'s
  instance lock). GTK is single-instance (GApplication). Two processes sharing a slot stay
  out of scope, and are stated as such.
- Changing the server's grace rule (#104) or family revocation (#116).
- A superseded client that is still alive can change the password, which signs out every
  device, the owner included. That's what a password change means; it's not prevented.
- One server under different URL spellings (host case, `:443`) gets different slots.
  They share no stored token, so they don't conflict.

## Review round 1

The adversarial review found the realistic trigger (the revoke of a discarded pair, not
the refresh race), four revoke paths, the double lock on the password flows, and the
WebSocket recovery missing from the covered paths. All are taken above. Also taken: the
exact local sign-out path, the definition of owner, a read-only `owns()`, and the
password-change and URL-spelling notes.

## Review round 2

All round-1 points are fixed; the owner definition (4) is stated rather than changed, and
that was accepted. New and taken: the revoke exemption keys on the token's origin (from the
slot) rather than on ownership alone, so a fresh login's pair that never installed is still
revoked. For the plan: `owns()` answers true when persistence is off. It must not be
written as `with_slot(..).is_some()`, which would stop every client without persistence
from refreshing; the last test catches that.
