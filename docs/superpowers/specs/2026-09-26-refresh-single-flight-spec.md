# One refresh at a time per stored session (#120)

Status: spec. Review dial: **Heavy** (auth: refresh-token rotation, family revocation).

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

## Done means

1. **A client that no longer owns its slot never refreshes.** The family is the owner's now.
   - Its refresh loop, `refresh_now`, and the pre-request refresh answer
     `NotAuthenticated`, and the client ends its session locally (`LoggedOut`).
   - It **revokes nothing**: revoking would end the owner's family (#116).
   - A client without persistence is unaffected.
2. **Refreshes of one slot are serialized process-wide.** Every refresh that presents a token
   from a slot runs under one async lock per slot name: restore's refresh, the refresh loop,
   and the password flows' refresh-lock sections. A refresh already in flight when
   ownership moves finishes before the new owner's restore reads the stored token.
3. **Ownership is re-checked under that lock**, right before the request. A client that lost
   the slot while it waited sends nothing (1).
4. Tests, each watched failing under a mutant:
   - two clients over one slot: the older is signed in and its refresh is held; the newer
     claims the slot and restores. Released, no token either client holds is retired by
     the other's refresh, and the test server records no reuse.
   - a superseded client's refresh sends no request, ends its session locally, and revokes
     nothing.
   - a client without persistence refreshes as before.

## Not doing

- Across processes. On the Mac, a second instance can't persist (`SessionPersistence`'s
  instance lock). GTK is single-instance (GApplication). Two processes sharing a slot stay
  out of scope, and are stated as such.
- Changing the server's grace rule (#104) or family revocation (#116).
