# Offline on the Mac (#62): spec

Status: spec, closed after review round 2. Review dial: **Heavy** (sign-out gains "Remove this
device's data", which deletes local data).

Core and the Swift API are done (#109, #113, #119, #123, #132, #149). GTK shipped the same
experience in #110 and #117, and its code (`clients/gnome/src/chat.rs`) is the reference:
where this spec says "as GTK", it means that code's behaviour. The Mac has a timeline
(#145) that reads the network only.

## When it applies

- **Local data is switched on per signed-in client.** When a sign-in succeeds on a client
  with session persistence on (`SessionPersistence.on`, which needs the Keychain group,
  and so the provisioning profile, #79), `SessionStore` calls
  `enableLocalData(slot:dataDir:)` on that client, with the same slot and directory. It's
  never called for an attempt that didn't sign in (the app makes a client per attempt). A
  `false` is logged once, and the app stays online only.
- **There's no app-wide "local data" flag.** This user's stores open in the background
  after that call, and can fail. So, as GTK does, every cached call is tried and
  `local.unavailable` means "use the network path". With persistence off, every call
  answers that, and the app behaves exactly as today.
- **Offline means offline mid-session.** A cold start without a connection lands on
  sign-in (`restore()` answers offline, and the app shows sign-in), and so does GTK.
  Opening signed in from the cache alone would need an offline restore in core, which is
  not in this spec.

## Done means

1. **Reading:**
   - **The channel list:** `startRealtime` and `listChannels` are tried separately (today
     one failure skips both). If `listChannels` fails, `cachedChannels()` fills the list,
     mapped from `FfiCachedChannel` into the list's model, with the cache's unread counts.
     If both fail, the list shows today's error.
   - **A channel:**
     1. Draw `cachedMessages(channelId, before: nil, limit: 50)`.
     2. If `needsNetwork`, call `loadHead`, then **re-read** the cache and draw it again.
     3. Then load the network history as today.
     Every page merges by id into the one timeline.
   - **Older pages:**
     1. Read `cachedMessages(before: oldest)`.
     2. If `needsNetwork`, call `loadOlder` and re-read.
     3. An empty page with `needsNetwork` false is the start (`atStart`).
     4. If local data is unavailable, page from the network as today.
   - **While messages from the cache are showing and the banner is up,** the timeline's
     network error ("Couldn't load messages") stays hidden.
2. **Sending:**
   - The composer tries `sendQueued(channelId, body, replyToId, clientId)` with a new
     lowercase UUID. On `local.unavailable` it falls back to `sendMessage`, as GTK does.
     The text clears when the call returns successfully. On any error it comes back, with
     the reason.
   - If an answer is lost (a network error on the fallback path), the retry reuses the
     same `clientId`.
   - **Bubbles** come from `pendingMessages(channelId)`, below the history. Their text
     follows GTK's `pending_text`:
     - `Pending`, `Sending` and `Accepted` read **"Sending…"**, dimmed (`Accepted`: the
       server has it, the cache hasn't caught up);
     - `Failed` reads **"Not sent"**, with **Retry** and **Delete**;
     - `message.reply_target_gone` reads **"Not sent: the quoted message was deleted"**,
       and its Retry becomes **"Send without the quote"**;
     - `not_found`, `authz.forbidden`, `http_403` and `http_404` read **"Not sent: you
       can't post here any more"**, still with Retry and Delete (a permission can come
       back).
     - GTK's file codes (`file.*`, `outbox.duplicate_file`, `outbox.snapshot_damaged`,
       `transfer.cancelled`) come with sending files, which is its own spec.
   - **No bubble for a message that's already in the timeline.** Every render filters out
     `clientId`s already shown, as GTK's `shown_client_ids` does. A later `Outbox` re-read
     that still lists it (as `Accepted`) never brings it back.
3. **Live** (`subscribeCacheEvents`; a lagged feed arrives as `Reset`):
   - `Outbox(channel)` re-reads that channel's bubbles.
   - `Channels(ids)` refreshes the badges (the open channel's stays at 0), and refills the
     open channel from the cache if it's among them.
   - `Users(ids)` redraws authors' names.
   - `Removed(ids)` drops those channels and closes the open one if it's removed. It also
     re-reads lost messages (5).
   - `Reset` reloads the list and the open channel, and re-reads lost messages (5).
4. **The offline banner** follows `subscribeCacheState()`: "Offline: showing messages saved
   on this Mac" while `offline`. It resets on sign-out.
5. **Lost unsent messages,** as GTK:
   - The app subscribes to cache events **before** `enableLocalData`, which can find a
     loss while opening the stores and emit `OutboxLost` before it returns.
   - It reads `outboxLost()` **after `enableLocalData` returns**, and on `OutboxLost`,
     `Removed` and `Reset`.
   - One alert at a time: "Some unsent messages on this Mac couldn't be recovered."
   - When it's dismissed, `acknowledgeOutboxLost(n)` for the `n` shown, then a re-check,
     so a newer loss shows once.
6. **Sign-out,** as GTK's sheet, when this client has local data enabled:
   - "Sign out of Brook?", with **"Remove this device's data"** ticked by default. That's
     the owner's decision (#46 §8).
   - Cancel is the default button, and Sign Out is styled as destructive.
   - The body text follows the checkbox, and the unsent warning shows only while it's
     ticked:
     - a count when `unsentCount()` is readable and non-zero ("3 messages haven't been
       sent yet. Removing this device's data deletes them.");
     - when this user's stores aren't known to be open, so the count can't be trusted,
       "Unsent messages on this Mac may be deleted." The stores count as open once a
       cached call has answered **successfully**, since each goes through core's
       `active()`, which succeeds only for the signed-in user's open stores. The flag lasts
       for the client's life, since the Mac drops a client at sign-out.
       `unsentCount()` answers 0 when the stores are closed.
     - With the stores open, 0 means no warning. Core also reads a count it failed to read
       as 0, and that small risk is accepted.
   - Ticked means `signOutAndForget()`. It signs out even if the erase fails. On an error,
     the app shows a warning ("Brook couldn't remove all of this Mac's data. Sign in and
     out again to retry."), then runs today's `signOutComplete()` check.
   - Unticked means `logout()`, as today.
   - Without local data enabled on this client, sign-out is as today, with no sheet.
7. **Other accounts' saved data** (#46 §8: one Mac's cache never crosses accounts):
   - At this user's first completed sync (`lastSyncedUnixMs` set), once per signed-in
     client, the app reads `otherLocalUsers()`.
   - If it isn't empty, it calls `wipeOtherLocalUsers()` and then says so, naming unsent
     messages as #46 §8 requires ("wiped after their unsent count is surfaced"):
     - "Another account's saved messages were removed from this Mac, including 3 unsent
       messages." (the sum of the known counts);
     - "…, which may have included unsent messages." when any count couldn't be read;
     - "Another account's saved messages were removed from this Mac." when every count is 0.

     The counts come from `otherLocalUsers()`, which gains an `unsent: UInt64?` per
     account, read from that account's outbox, or nil if it can't be read. That's a small
     core and binding change, in its own PR, which GTK can use too.
   - An account counts as other by server and user. Signing in to a second server removes
     the first server's saved data, unsent messages included. That follows from the same
     decision, and the sheet from item 6 was the chance to keep it.
8. **Tests** (models against fakes, no server; each watched failing under a mutant):
   - Reading:
     - offline, `listChannels` fails so cached channels are shown; both failing shows the
       error;
     - `startRealtime` failing doesn't skip `listChannels`;
     - the cache is drawn before the network, and `needsNetwork` loads then re-reads;
     - cached paging with `atStart` from an empty, complete page;
     - with local data unavailable, the network path as today;
     - the network error hidden while cached messages show offline.
   - Sending:
     - a fresh lowercase id each time;
     - `local.unavailable` falls back to `sendMessage`;
     - an error restores the text;
     - every state's text, including `Accepted`, and the actions for each failure code.
   - Duplicates: after a message with the `clientId` arrives, an `Outbox` re-read listing
     it as `Accepted` shows no bubble.
   - Events:
     - `Outbox` re-reads;
     - `Channels` refreshes badges and keeps the open one at 0;
     - `Removed` closes the open channel;
     - `Reset` reloads, including the lagged-feed case.
   - The banner follows the feed and resets on sign-out.
   - Lost messages: one alert, acknowledged with its `n`, then a newer loss shows.
   - The order: subscribe, then `enableLocalData`, then `outboxLost()`, so a loss found
     while opening is shown.
   - A sign-in during an erasing sign-out waits for it before enabling local data.
   - Sign-out:
     - ticked by default;
     - the text follows the tick;
     - the counted warning, the "may be deleted" warning when the stores aren't known to
       be open, and no warning while unticked;
     - a failed erase gives the warning;
     - no sheet without local data.
   - Other accounts: nothing before the first sync; the alert and the wipe when there are
     others; nothing when there are none.
   - A live check with the app needs #79, so it's done when that lands.
     `ChatIntegrationTests` already covers the queue, replies and files through BrookCore,
     with an in-memory slot.

## Not doing

- An offline cold start (it needs core's restore to open from the cache).
- A Debug-only in-memory Keychain slot to run the app with local data before #79. It
  would be a second path around the gate.
- Sending files from the Mac (its own spec, next), previews in the row, and "Keep
  available offline": after #79.
- Any other core change (the one exception is the unsent count per other account,
  item 7).

## Where it fails

- **Cached and network timelines disagree:** one model merges by id, and a tombstone wins
  over a late copy.
- **A queued message shows twice:** the shown-`clientId` filter runs on every render, and
  core never resends a `clientId` the server has accepted.
- **Unsent messages lost at sign-out:** the warning shows while the box is ticked, and says
  "may" when the count can't be trusted. Cancel is the default.
- **Two clients for the same user:** local data is enabled only on the client that signed
  in. The previous attempt's client is dropped, and core's single-flight (#120) applies to
  its session.
- **A sign-in right after a sign-out that's still erasing:** the old client's
  `logout`/`signOutAndForget` runs in a task that keeps its stores open, and there's no
  file lock on `stores`. So the next sign-in awaits that task before it calls
  `enableLocalData`. There's a test for it.

## Review round 1

Two reviewers (vibe, and a second model standing in while codex is unavailable).

Taken, from the second reviewer (all eleven points):
- Offline is mid-session only. Local data is decided per call, with a fallback.
- The lagged feed arrives as `Reset`, and there's no Resync.
- GTK's pending texts and codes, including `Accepted`.
- The shown-`clientId` filter.
- Sign-out: an untrustworthy count, an erase that fails, and GTK's sheet behaviour.
- The other-accounts alert, and the second-server consequence stated.
- Cached paging re-reads the cache after each load.
- Mapping `FfiCachedChannel`, separating `startRealtime` from `listChannels`, hiding the
  network error while cached messages show, keeping the open badge at 0.
- Enabling local data per signed-in client.
- The `clientId` reused on retry, `Users` events, and lost-message checks on `Removed` and
  `Reset`.

Taken, from vibe:
- Both sources failing shows the error.
- A count that can't be trusted is never shown as 0.
- One lost-message alert at a time, acknowledged with its own `n`.

Rebutted (vibe):
- **Untick "Remove this device's data" by default.** The owner decided it's ticked (#46
  §8). What guards unsent messages is the warning shown while it's ticked, plus Cancel as
  the default button.
- **Sign-out racing a `sendQueued` in flight.** The sheet is modal, and `sendQueued`
  saves before it returns. A send is either saved, and so counted in the warning, or its
  text is still in the composer. There's no in-between to lose.

## Review round 2

Vibe: no objections, and it accepts both rebuttals. The second reviewer confirmed the
round-1 fixes and raised four points, all taken:
- Retry and Delete for the "can't post here" codes, exactly as GTK, with GTK's texts, and
  the file codes named as out of scope.
- "Answered" means successfully, and a 0 from open stores means no warning, as a stated
  risk.
- Subscribe before enabling local data, and read losses after it returns.
- The next sign-in waits for an erasing sign-out.

Nothing disputed: closed.

## Review by the server side

Taken: the other-accounts notice now names their unsent messages before the wipe, as
#46 §8 requires. That needs `otherLocalUsers()` to carry a count, a small core change in
its own PR. GTK's #110 has the same gap, and the same API closes it.
