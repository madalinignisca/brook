# Mac: ownership offers: plan

Spec: `2026-09-26-mac-owner-offers-spec.md`. Standard. One PR after #191 lands (it needs
its bindings).

1. **`ChannelRow.ownerOffers`**, from `FfiChannel` and `FfiCachedChannel`.
   `ChannelsModel.offerToMe(row)` returns the offer naming `me`, or nil while `me` is
   unknown.
2. **`MembershipClient`** gains `offerOwnership`, `withdrawOwnershipOffer`,
   `acceptOwnership` and `declineOwnership`. `FfiBrookClient` already has them, and
   `FakeMembership` records them.
3. **`ChannelPowers`** takes `offers` and gains:
   - `canOffer(m)`: an owner or admin, beside a non-owner who isn't you and has no pending
     offer;
   - `pending(m)`: an offer names them.
4. **`MembersModel`**:
   - `removing` becomes `busy` (one member action at a time: remove, offer or withdraw);
   - `offer(handle:)` and `withdraw(userId:)` with the spec's texts; `already_owner` and
     `offer.not_found` are silent.
5. **`OfferAnswerModel(channelId, title, offerer, client)`**:
   - `accept()` and `decline()`, one at a time;
   - `done` on success or `offer.not_found`;
   - `error` and `canDefer` after any other failure.
   - `offererName(offer, members)` falls back to "Someone".
6. **Views:**
   - the sidebar badge "Owner?";
   - Members gets "Make Owner…" (with a confirmation) and "Owner offered · Withdraw";
   - `SignedInView` presents `OfferAnswerSheet` with `isPresented` computed as: the
     selected row has an offer to me, and it isn't deferred for this opening. `deferred`
     clears when the selection changes. The sheet uses `interactiveDismissDisabled()`.
7. **Tests**: each spec §3 bullet, each watched failing under a mutant.

**If it stops halfway:** every piece shows only when `ownerOffers` is non-empty, which
needs #190 on the server. Until then nothing changes.

**Where it fails:** the presentation binding is computed. SwiftUI sets it false only via
dismissal, which is disabled, or when the offer goes; that's the "follows the offer" rule.

## Plan review, round 1 (vibe; Standard)

Taken:
- **No presentation loop.** The binding's setter only records `deferredOffer = channelId`
  and never clears it. Only a change of selection clears it (a new opening), so "Ask Me
  Later" can't immediately re-present.
- **The answer model lives in `SignedInView`** (`@State answering: OfferAnswerModel?`),
  made when the sheet is presented for a channel and dropped when the offer goes or the
  selection changes. Its `error` and `canDefer` survive re-renders.
- **The deferral is per channel** (`deferredOffer: String?` holds the channel id), so
  deferring one never blocks another channel's question.

Closed.

## Implementation review, round 1 (vibe; Standard)

Taken:
- **The binding's setter deferred on any close.** The sheet can't be dismissed, so SwiftUI
  only closes it itself (offer gone or answered). The setter is now a no-op, and "Ask Me
  Later" is the only deferral.
- **A new offer after "later" in the same opening.** Deferrals are keyed by channel,
  offerer and time, so a new offer asks. Tested.

Not unit-tested: the views and `SignedInView`'s wiring (the badge, the popover buttons,
the sheet binding). `OfferPrompt` holds the presentation rules and is tested.

Measured: 20 mutants caught, 1 equivalent ("unknown me": with no id the code compares
against "", which no offer carries). 260 Mac tests pass.

## PR review (GTK side): CHANGES, taken

- **The app never passed the channel's offers into `ChannelPowers`**, so Withdraw and "Owner
  offered" never showed. The models were tested with offers set directly, which hid it.
  Powers are now built by `ChannelPowers(row, me:, isAdmin:)`, which carries the row's
  offers, and a test builds them the way the app does. Caught under a mutant.
