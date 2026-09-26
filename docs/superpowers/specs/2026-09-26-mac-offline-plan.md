# Offline on the Mac (#62): plan

Spec: `2026-09-26-mac-offline-spec.md` (#160, closed, with the other-accounts count from
#163). Heavy. One PR of Mac code. The only core dependency is #163 (`FfiLocalUser.unsent`).

## Steps

Each step builds and passes the full Mac suite on its own. Local data is switched on only in
step 6, together with everything that must come with it: the sheet, the notice and the
wipe.

1. **`Chat/OfflineClient.swift`** (protocol) and the fakes.
   - `OfflineClient` covers the cached calls the models make, as the spec lists them.
     `extension FfiBrookClient: OfflineClient {}`.
   - `FakeChat` implements it. `FakeClient` (the `FfiBrookClient` subclass that
     RestoreTests and SessionStoreTests use) overrides `enableLocalData`,
     `subscribeCacheEvents`, `subscribeCacheState`, `outboxLost`, `acknowledgeOutboxLost`,
     `otherLocalUsers`, `wipeOtherLocalUsers`, `signOutAndForget`, `unsentCount`,
     `cachedChannels` and `logout`. Each one records the order it was called in, and none
     reaches Rust.
2. **`ChannelRow`** replaces `FfiChannel` in the list: id, kind, name, archived and unread.
   - It's built from `FfiChannel` (network), with the unread count filled from
     `cachedChannels()` when that answers, or from `FfiCachedChannel`.
   - `CallCenter.join` and `performJoin` take a `channelId`, since they only use `.id`. So
     do CallModelTests, ScreenshotRenderer and the `channel()` helper.
   - The sidebar shows the unread badge. The compiler finds every remaining use.
3. **`TimelineModel`**, reading and merging:
   - `load()`: the cached page, then `loadHead` and a re-read if `needsNetwork`, then the
     network history.
   - `loadOlder()`: the cached page before the oldest, `loadOlder` and a re-read if
     `needsNetwork`, and `atStart` on an empty, complete page. On `local.unavailable` it
     pages from the network.
   - **The merge no longer depends on arrival order:**
     - a `deleted` set of ids, including messages not yet shown, is applied to every page;
     - `editedAt` guards only the body and its edited mark. A missing `editedAt` is older
       than any timestamp, and a body is replaced only by one whose `editedAt` is newer, or
       equal when both are missing. Every other field (the author's name and handle, the
       reply excerpt, attachments, deleted) always takes the incoming copy, so a `Users`
       refill renames authors;
     - `showingCached` hides the network error while the feed is offline.
   - `refill()` re-reads the open channel's cached head, for the feed's `Channels` and
     `Users` events and for `Reset`.
4. **`PendingModel`**, per open channel:
   - `all` is the last `pendingMessages` read. Each read carries a generation number, and
     only the newest applies.
   - `visible` is computed as `all` minus the `clientId`s in the timeline, so it updates as
     soon as a message arrives.
   - The texts and actions follow the spec's table (GTK's). `retry`, `retryWithoutQuote`
     and `delete` call core, then re-read.
5. **`ComposerModel`:**
   - `send()` calls `sendQueued` with a `clientId`. The id is kept, as GTK does (#162),
     only while channel, body and reply target are unchanged since a failed `sendQueued`
     (any error other than `local.unavailable`). Otherwise the id is new, lowercase.
   - Success clears the text and asks `PendingModel` to re-read. The receipt carries no
     message: the bubble shows at once, and the message arrives by events.
   - On `local.unavailable` it calls `sendMessage` as today, with no id, and keeps today's
     "may not have been sent" wording.
   - Any error restores the text.
6. **`CacheFeed` and `SessionStore`** (local data switched on):
   - `CacheFeed` (`@MainActor`, one per signed-in client) holds both subscriptions. A
     bridge delivers in order through `DispatchQueue.main`, as `EventBridge` does. It
     exposes `offline`, `lastSynced`, `lostAlert` and `notice`, and models register with
     it: `ChannelsModel`, and the open `TimelineModel` and `PendingModel`.
     - `Channels` refreshes badges and refills the open timeline. Each list read has a
       generation number, and ids that `Removed` dropped are filtered out of every later
       read.
     - `Users` refills.
     - `Removed` drops the channels, closes the open one, and re-checks losses.
     - `Reset` reloads the list and the open channel, and re-checks losses.
     - `OutboxLost` re-checks losses.
     - Lost messages: one alert, then `acknowledge(n)` and a re-check.
     - Other accounts: once per feed, at the first `lastSynced`, read `otherLocalUsers()`.
       If it isn't empty, wipe and set `notice`, with the unsent sum, or "may have
       included" when any count is nil.
   - `SessionStore.finishSignIn`, when `persistence` is `.on`, starts one task for its
     attempt `mine`:
     1. wait for the previous sign-out task and any previous enable task, up to 30 s. The
        wait resumes on whichever comes first, the task's end or the 30 s, through a
        continuation. It never joins a stuck task, so a hung sign-out can't hold it;
     2. `guard mine == attempt` (and not cancelled) comes **before** anything is logged.
        Only a real timeout logs once and stays online-only;
     3. create the feed, which subscribes;
     4. `enableLocalData`;
     5. `guard mine == attempt` again: if it moved on, drop the feed;
     6. on `true`: `localDataEnabled = true` and `feed.checkLost()`. On `false`: log once.
   - `end()` cancels that task, stops the feed and sets it to nil, so the banner resets.
   - `SignedInView` gets `store.feed`.
   - **The sign-out sheet** (`SignOutModel`, testable, with a view):
     - on open, it probes with `cachedChannels()`. Success means the stores are open, so
       it reads `unsentCount()`; a failure means "may be deleted";
     - `removeData` is ticked by default, and the text follows it;
     - Cancel is the default, and Sign Out is destructive.
     - `signOut(removeData:)`:
       - ticked runs `signOutAndForget`, and an error sets the removal warning;
       - unticked runs `logout`;
       - either way the task is kept as `signOutTask`, and `signOutComplete()` is checked
         afterwards;
       - the removal warning wins over the incomplete one, and both keep the
         `before == signIns` guard.
     - The sheet shows whenever this client's enable succeeded **or is still running**.
       Ticked with an enable still running waits for it before `signOutAndForget`, so data
       it opens is removed too. Only a client with persistence off, or an enable that
       answered `false`, gets today's direct sign-out.
7. **Views:** the offline banner, the pending bubbles, and alerts for lost messages and the
   other-accounts notice.
8. **Tests**, one or more per spec bullet, each watched failing under a mutant:
   - `OfflineTimelineTests`:
     - cache first, then `loadHead` and a re-read;
     - paging and `atStart`;
     - a delete before its page keeps the message deleted;
     - edits in both arrival orders: a stale cached copy (no `editedAt`) never replaces the
       edited body, and an edit arriving after a stale copy replaces it;
     - a `Users` refill updates an author's name with no edit;
     - the network error hidden while offline;
     - `local.unavailable` means the network path.
   - `PendingModelTests`:
     - the text and actions for every state and code, including `Accepted`;
     - `visible` drops a bubble once its `clientId` arrives, with no re-read;
     - an older read never overwrites a newer one.
   - `ComposerQueueTests`:
     - a lowercase id;
     - the id reused only for the same channel, text and quote after a failure;
     - the fallback on `local.unavailable`;
     - text restored on error.
   - `ChannelsOfflineTests`:
     - the split start: realtime failing still lists;
     - cached channels when the list fails, and the error when both fail;
     - unread counts filled after a network list;
     - the open channel's badge stays 0;
     - `Removed` ids filtered from a later, older read.
   - `CacheFeedTests`:
     - events delivered on the main thread, in order, sent from a background queue;
     - the banner, lost messages, the other-accounts notice (counts, and nil), and
       nothing before the first sync;
     - `Reset` reloads.
   - `SessionStoreOfflineTests` (`FakeClient`):
     - subscribe, then enable, then `outboxLost`, in that order;
     - no enable for a failed attempt;
     - a late enable after sign-out is dropped;
     - a sign-in waits for an erasing sign-out, with a 30 s cap: a hung sign-out ends
       online-only after 30 s, not forever. A sign-out during the wait (cancellation) logs
       no timeout;
     - a sign-out while enable is running shows the sheet, and a ticked one waits for the
       enable before removing;
     - `end()` resets the feed.
   - `SignOutModelTests`:
     - ticked by default, and the text follows the tick;
     - the counted warning, and "may" when the probe fails;
     - a failed erase gives the warning, and it wins over the incomplete one;
     - no sheet without local data.

## Where it fails

- **The `ChannelRow` change** touches the call code. That's step 2, on its own, with the
  existing call tests as the check.
- **A 30 s wait** delays local data after a hung sign-out. The app is online-only
  meanwhile, which is safe.
- **Main-thread delivery:** both bridges hop to `DispatchQueue.main`. A test sends events
  from a background queue and checks the order.

## If it stops halfway

Steps 1 to 5 change nothing visible while local data is off, and it stays off until step 6.
Step 6 brings the enable, the sheet, the notice and the wipe together, so there's never a
wipe without its notice, or a filled cache without a way to remove it.

## Plan review, round 1

Two reviewers (vibe, and a second model standing in while codex is unavailable).

Taken, all ten from the second reviewer:
- `FakeClient` grows in step 1.
- Enable, sheet, notice and wipe come together in step 6.
- `storesOpen` is replaced by a probe when the sheet opens.
- Order: a deleted-id set, the newer `editedAt` wins, generation numbers on whole-list
  reads, and removed ids filtered out.
- `visible` pending is computed.
- The late enable is guarded by `mine`, and `end()` tears down the feed.
- The waits are capped at 30 s, and a previous enable is also awaited.
- The `clientId` rules follow #162, and a success re-reads the pending list.
- `join` takes `channelId`, unread counts come after a network list, and the badge view is
  in.
- Users, Reset and lost-message wiring; `SignOutModel`; the warning precedence; the feed
  passed to `SignedInView`.

Taken from vibe: main-thread delivery, and a test that sends events from a background queue.

Rebutted (vibe):
- **"Sign-out unavailable without local data."** It's today's direct sign-out. The sheet
  appears only when there's data to remove.
- **"The sheet's text after the other-accounts wipe."** That wipe never touches this
  user's data, which is what the sheet is about.
- **"The wipe never runs if enable fails."** Then this client has no local data, and
  `otherLocalUsers()` answers `local.unavailable`. There's nothing to wipe until an enable
  succeeds.

## Plan review, round 2

Vibe: no objections, and it accepts the rebuttals. The second reviewer confirmed the
round-1 fixes (the deleted-id set doesn't leak), and raised three new points, all taken:
- **The merge rule.** A missing `editedAt` counts as older, and it guards only the body, so
  names and excerpts always update. Tests cover both edit orders and a `Users` rename.
- **The 30 s wait** resumes on whichever comes first and never joins a stuck task. A
  cancellation is checked before any timeout log.
- **A sign-out while enable is running** offers the sheet, and removal waits for the enable.

Nothing disputed: closed.

## Deviations in the implementation (for the reviewers)

- **`Users(ids)` overlays current names; it doesn't re-read messages.** Cached message rows
  keep the name they were stored with (settled in #164). So `TimelineModel.refreshAuthors`
  fetches `cachedUsers(ids)` into `authorNames`, and every row reads its author through it,
  including rows drawn later (GTK does the same since #166).
- **`localDataWait` is an init parameter** (30 s by default), so the hung-sign-out test runs
  at 300 ms.
- **One existing test's data changed:** `testEditsReplaceAndDeletesStay` now gives its edit
  an `editedAt`, as every server edit carries, since the merge changes a body only for a
  newer `editedAt`.

## Measured

- 175 Mac tests pass (`build.sh test`).
- 23 mutants, one or more per spec bullet, each caught by the test aimed at it.
