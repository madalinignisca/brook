# Mac: channel ownership offers (#190, #191): spec

Status: spec, closed after review round 1. Review dial: **Standard**. The UI is new, built on reviewed calls (#191). The
permissions are the server's (#190, reviewed Heavy there), and the UI only offers what the
server allows.

The owner's UX, as decided for #190: making someone an owner works like adding a member.
The recipient's channel is highlighted while an offer to them is pending, and opening it
shows an Accept/Decline dialog that can't be dismissed. Accepting adds an owner, and the
offerer stays one. There's one offer per member and no expiry, and the offer ends if either
side leaves.

## Done means

1. **Offering** (the Members popover, #187):
   - Beside a member who isn't an owner, for an owner or a global admin, "Make Owner…"
     opens a confirmation: "Offer <name> ownership of <title>? They'll be asked when they
     next open it." It calls `offerOwnership(channelId, handle)`.
   - While an offer to that member is pending (from `ownerOffers`), the row says "Owner
     offered" and has "Withdraw" instead, which calls `withdrawOwnershipOffer`.
   - The list updates from `channelUpdate`, never optimistically.
   - Errors:
     - `channel.already_owner` and `offer.not_found`: silent, since the update on its way
       redraws the row;
     - `authz.forbidden`: "You can't offer ownership here.";
     - `channel.not_member`: "They're no longer a member.";
     - otherwise: "Couldn't do that. Try again."
2. **The recipient:**
   - In the sidebar, a channel whose `ownerOffers` contains this user shows a "Owner?"
     badge (accent colour) and sorts no differently.
   - Opening that channel presents a sheet: "<offerer's name> offered you ownership of
     <title>." Owners can rename, archive and delete it, and remove members. The sheet has
     Accept and Decline and can't be dismissed otherwise (`interactiveDismissDisabled`,
     no Cancel).
   - Accept or Decline calls the server, and the sheet closes when the call succeeds, or
     on `offer.not_found` (withdrawn meanwhile, or the offerer left). On another failure
     it stays open with the error ("Couldn't answer. Try again.") so the question is
     still asked.
   - **The one way out without answering:** once an answer has failed (any failure but
     `offer.not_found`), the sheet adds "Ask Me Later". A window sheet blocks the whole
     window, so otherwise an offline user, or one facing a server error, couldn't read
     anything. It closes the sheet until that channel is next opened.
   - **The sheet follows the offer:** it's presented only while the open channel's
     `ownerOffers` names this user. A `channelUpdate` that drops the offer (withdrawn, or
     the offerer left) closes it, so a stale offer is never shown.
   - The sheet appears again whenever the open channel still has an offer for this user,
     including after a relaunch, since the offer lives on the channel.
   - The offerer's name comes from the channel's members, or "Someone" if the offerer is
     no longer listed.
3. **Tests** (models against fakes, each watched failing under a mutant):
   - Offering:
     - "Make Owner" only for an owner or admin, and only beside a non-owner without a
       pending offer;
     - "Withdraw" beside a pending offer;
     - the calls and error texts;
     - `already_owner` and `offer.not_found` are silent.
   - The recipient:
     - the badge shows exactly when an offer names this user;
     - the question is asked for the open channel only when the offer is to this user;
     - Accept and Decline call once, and a second tap while running sends nothing;
     - success and `offer.not_found` close the sheet; another error keeps it open with its
       text;
     - "Ask Me Later" only after a failed answer, and asked again at the next opening;
     - an update that drops the offer closes the sheet;
     - the offerer's name, with the "Someone" fallback.

## Not doing

- Notifications for offers (the highlight is the notice, as decided).
- Demoting an owner, or transferring ownership without the recipient's consent.
- iOS.

## Where it fails

- **The offer is withdrawn while the sheet is open:** the `channelUpdate` closes it. If
  Accept was already on its way, it answers `offer.not_found`, which also closes.
- **The sheet on a channel with no network:** the answer fails, "Ask Me Later" appears,
  and the question comes back at the next opening. Only a failed answer unlocks it, so
  the choice stays forced whenever it can be made.

## Review round 1 (vibe; Standard)

Taken:
- **Trapped after a non-network error.** "Ask Me Later" appears after any failed answer,
  not only a network failure.
- **A stale offer while the sheet is open.** The sheet is bound to the open channel's
  offers, so a `channelUpdate` that drops the offer closes it. Tested.

Closed.
