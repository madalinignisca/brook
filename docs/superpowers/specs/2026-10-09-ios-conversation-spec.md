# iOS: open a conversation, read it, send to it (#337)

Review: `brook-reviewer`. No server, protocol or auth change. Shared Swift that the Mac app also
builds changes, so the Mac's tests must stay green. One small core and binding change comes
first: the direct send carries a `client_id` (§7, decision 2).

The #335 dependency (blank screen after sign-in on a real iPhone) is met: #338 fixed it.

Changed at plan stage (2026-10-09), from the plan's §5 decisions: in the agent's checks the
second account acts through the API, and "shows on the Mac" moves to the owner's iPhone check
(Done, §7 decision 1); iOS closes the conversation when its row leaves the list, and `closed`
stays as it is (§3, §4, §5, Done 10); GNOME and KDE pass no `client_id` for now (§4 Sending,
§7 decision 2); every newest-page fetch on iOS, `resync` included, goes through the gap
check, so that known limit is gone (§4); what "joins" means for a second `ready` during a
page-back (§4, Done 10); `ComposerView.replyBanner` stays on the Mac (§5). The owner approved
all six on 2026-10-09. Also from the plan: the gap's anchor is
taken when the re-read is asked for, and a replace keeps shown messages newer than what it
fetched and restarts a running older page (§4 Staying live, Done 10).

Changed after the first review: a re-read now marks read (§4 Read state; Done 6, 10); the gap
rule pages back before it replaces, and resets what it must (§4 Staying live); duplicate sends
are open question 2; the composer and its file code move whole, with the sharing rule relaxed to
"no AppKit in shared code" (§5); checks run on the agreed server, not production by default
(Done, question 1); the scroll rule and the jump-to-latest button are proposed (question 3);
the keyboard dismisses by scrolling; more unit tests (Done 10); the docs that drift (Done 12);
`closed` never cleared (§5 risks).

Changed after the second review: the checks run in the simulator on loopback, and the owner's
real-iPhone check runs from the Home Screen (question 1, Done); the page-back joins the head
fetch (§4); reaction marks clear only on `ready` (§4, Done 10); the Mac's direct send keeps its
draft id if question 2 is yes (§5 risks); `TypingSearch.swift` moves whole (§5).

As built, step 3 (the shared timeline's re-read, behind `rereadOnReady`, off on the Mac): the
gap anchor is taken when the re-read is asked for (`ready`, `resync`, return to the foreground),
not when the fetch runs; a replace keeps the shown messages newer than the newest one fetched, so
a live message or the user's own send that landed during the page-backs is not lost; and an older
page that started before a replace is dropped when its answer lands, or before it is sent, and
`loadOlder` asks again from the new oldest message. Two `ready` events delivered back to back,
before the first fetch has started, are served by that one fetch (the drain's existing rule); one
that arrives while a fetch runs gets exactly one more.

Changed after approval: the owner took the recommended answer to each open question; §7 records
them, and the spec now reads as decided: the checks' servers (Done), `client_id` on the direct
send (§4 Sending, §5, Done 7, 10), and the scroll rule with the jump-to-latest button (§4, Done
4).

## 1. Problem

The iPhone app signs in and lists channels and DMs (#272), but tapping a row opens nothing. A
person on an iPhone can see that a channel has a mention for them and cannot read it, answer
it, or look back at what was said.

The Mac app already has a full conversation screen. Its message logic (paging, merging live
events, marking read, sending text) is plain Swift that iOS needs too. Copying it would give
two copies to keep in step, which CLAUDE.md section 7 rules out.

## 2. Goal

From the list, a person opens a channel or DM, reads its messages, scrolls back to its start,
and sends a text message. Both the list and the open conversation stay live. It looks like the
iOS the user has: a system navigation push, a plain scrolling list of messages, a text field
with a Send button above the keyboard. Nothing branded.

Done when, each checked and written in the PR. Items 1 to 8 run as §7 decision 1 says
(the simulator against the local stack, then a real iPhone against the owner's server). "The
Mac" below is the second account. In the agent's simulator checks, that account acts through
the server API, not the Mac app. In the owner's iPhone check it is the Mac app, and the parts
that need the Mac app ("shows on the Mac", Done 3 and 7) are checked there:

1. Tapping a channel row opens its conversation with `#name` as the title; tapping a DM row
   opens it with the other person as the title. The newest messages show at the bottom within
   a few seconds. Back returns to the list.
2. In a channel with more than 100 messages, scrolling up loads older messages until "This is
   the start of the conversation." shows. What the user is reading does not jump when an older
   page lands. With the network off while scrolling up, a Retry row shows instead of a spinner;
   with the network back, Retry loads the page.
3. A message typed on the phone and sent with Send shows once in the phone's conversation and
   shows on the Mac without any action there. Send is disabled while the box is empty or only
   spaces. The box and the newest message stay visible above the keyboard; scrolling the
   messages dismisses the keyboard.
4. While the conversation is open on the phone, the Mac sends a message: it shows on the phone
   within a few seconds, without reopening. The Mac edits it: the phone shows the new text and
   "edited". The Mac deletes it: the phone shows "Message deleted". The Mac reacts to a
   message: the phone shows the reaction and its count. Scrolled up into older history, a new
   message from the Mac does not move the view, and a jump-to-latest button shows; tapping it
   goes to the newest message. At the bottom, a new message scrolls into view. A message sent
   from the phone always scrolls into view.
5. The Mac mentions the phone's account in a channel; the row shows `@1`. Opening the channel
   clears the badge. Back on the list, pull to refresh: the badge stays gone (the server's read
   marker moved, not only the phone's copy). A new mention from the Mac after going back shows
   the badge again.
6. With a conversation open, press the side button (or go to the Home Screen), send three
   messages from the Mac (one mentioning the phone's account), wait 30 seconds, come back: the
   three show without reopening and without a pull. Go back to the list and pull to refresh:
   that channel shows no mention badge (the re-read marked them read).
7. With the network off, Send puts the text back in the box with "It may not have been sent:
   check the conversation before sending again." With the network back, Send again: the
   message shows once, on the phone and on the Mac.
8. The Mac archives the channel while it is open on the phone: within a few seconds the box is
   replaced by "This channel is archived. An owner or admin can unarchive it." The Mac removes
   the phone's account from a channel open on the phone: the phone goes back to the list, and
   the channel is gone from it.
9. Each lifecycle step above (open, back, lock and unlock, Home Screen and back) is also checked
   on a real iPhone (iOS 26), not only in the simulator: on the device a `.task` on an empty
   `Group` never ran while the simulators ran it (#335).
10. `clients/ios/build.sh test` exits 0, and its unit tests cover:
    - opening and leaving a conversation sets and clears the list's open channel and its
      message events;
    - the open conversation re-reads its newest page on every `ready` and when the scene
      becomes active (the scene-phase wiring, as `ForegroundReload` is tested for the list);
    - a re-read with the app active marks the newest message read; with it not active, it
      leaves the read owed;
    - a re-read that overlaps merges; one that does not pages back and merges when a page
      reaches the anchor; one that never reaches it within the limit replaces the shown
      messages but keeps those newer than what it fetched, with `atStart` and `olderFailed`
      reset, and an older page that was running dropped and asked again from the new oldest
      message; the anchor is the newest message shown when the re-read was asked for, not when
      it ran; a second `ready` during the page-back starts no fetch beside it, and exactly one
      more newest-page fetch follows when it ends;
    - the conversation closes when its row leaves the list, both on a live removal and on a
      list re-read after a reconnect;
    - a `ready` clears the reaction order marks; a foreground re-read with the socket still up
      does not;
    - a send that fails puts the text back with the right message;
    - a direct send passes a `client_id`, and sending the same text again after a failure
      passes the same one; changed text gets a new one;
    - the view follows a new message only when not scrolled away, or when it is the user's
      own (`ScrollToLatest.isAway`).

    `cargo test` covers core's `send_message` sending the `client_id` it is given, and none
    when given none.

    `clients/macos/build.sh test` still passes, and covers that the re-read flag is off on the
    Mac: a `ready` there only retries a failed head, as today (mirroring the
    `rereadOnReconnect` tests of the list).
11. Code used by both apps exists once in the repository: no Swift file or function is copied
    between `clients/macos` and `clients/ios`.
12. The written record matches the code: `clients/ios/README.md` says what the conversation
    screen does and does not do yet (§3), drops "nothing opens a channel", and states the real
    reason for no unread counts (the server sends `unread_count`; the Apple binding drops it);
    the comments in `clients/ios/Brook/ChannelListView.swift` that say rows open nothing and
    that the plain count exists only in the Mac's cache are corrected the same way.

## 3. Scope and non-goals

| Item | In or out | Why |
|---|---|---|
| Open a channel or DM, read, scroll back, send text | In | The issue. |
| Live: new, edited and deleted messages, reactions changing | In, shown only | The shared model already applies these events. Hiding them would show a different conversation from the Mac's. |
| Mark read when a conversation is open and the app is active | In | Without it the mention badge never clears, and other devices of the same account keep counting the messages as unread. The shared model already does it. |
| Archived channel: no box, a note instead | In | The server refuses the send (`403`); a box that always fails is worse. The shared composer has the read-only switch. |
| Removed from the open channel: back to the list | In | The list already drops the row; iOS closes when the row is gone (§4). A stale screen whose sends fail is worse. |
| A reply's quote line, a file's name and size, the "edited" mark, the tombstone | In, shown only | The data is in every message. A message with a file and no caption would otherwise look empty. |
| Copying a message's text | In | Text selection on, the system's own long-press. One modifier, as the Mac. |
| Follow new messages only at the bottom; jump-to-latest button | In | §7, decision 3. The Mac has the button (#284). |
| Paste of plain text into the box | In | The system text field does it; nothing to build. |
| The typing signal sent while typing | In | The shared composer sends it already; turning it off would need a switch. |
| Showing who is typing | Out | Nice later; its own issue. |
| Attachments: send, open, save, image previews, pasting an image or file | Out | Sending files goes through the local outbox, which iOS does not have (§4). Open, save and previews need the file cache or a share sheet design. The file code is compiled on iOS (§5) but no iOS screen calls it. |
| Reply, edit, delete, react from the phone | Out | Each is an action with its own UI and errors. The first screen reads and sends. Its own issues. |
| Threads | Out | Brook has no threads, only reply quotes. |
| Plain unread counts in the list | Out | The binding drops the server's count (Done 12). Its own small issue: one binding field and how `ChannelRow` picks its count. |
| A tint on messages that mention you; date separators; links; Markdown | Out | Nice later. The Mac has the tint, not the rest. |
| Calls from the conversation | Out | `BrookMedia` and WebRTC are not built for iOS. The list's call badge stays. |
| Notifications, local or push | Out | Push (APNs, `POST /devices`) is its own work. Messages that arrive while the app is in the background show when it comes back (Done 6). |
| Search, channel info, members, creating or joining channels | Out | Not needed to read and send. |
| A draft kept after leaving the conversation | Out | The box belongs to the open screen; going back drops it, as on the Mac. |
| Local data (#62): cached history, offline outbox | Out | iOS runs with `makeFeed: nil` (§4). |
| iPad layouts | Out | iPhone only, as #272. |

## 4. Behavior

### Opening and leaving
- Tapping a row pushes the conversation onto the list's navigation stack. The title is the
  row's label (`ChannelsModel.title`), shown inline.
- While it is open, the list knows it as the open channel: its badge stays clear, and the
  message events go to it. Going back stops both. The list's own updates keep running
  underneath, so a rename while the conversation is open shows on return.
- Each opening starts from nothing: there is no cache to show first.
- Removed from the channel: the conversation closes and the list shows again, as soon as the
  channel's row leaves the list. That covers both ways iOS learns of it: the live
  `channel.delete` event, and the list's re-read after a reconnect, which drops the row without
  any event. iOS does not watch `ChannelsModel.closed` (§5 risks).

### Reading and history
- The newest page (50 messages, the server's default) loads first, oldest at the top, and the
  view starts at the bottom.
- Each row shows the author (the shared `PersonName` rule, display names, since iOS has no
  Show usernames setting), the time, the body, and when present: "edited", the reply's quote
  line, each file's name and size, the reactions with their counts. A deleted message shows
  "Message deleted". Wording follows the Mac.
- Scrolling to the top asks for the page before the oldest message, 50 at a time, until the
  server returns none: "This is the start of the conversation." A failed page shows "Couldn't
  load older messages. Retry".
- A newest page that fails shows "Couldn't load messages." It is tried again on the next
  `ready` or when the app returns to the foreground.
- Scrolling the messages dismisses the keyboard, the iOS habit.
- New messages (§7, decision 3): the view follows one only when it is already at the bottom
  (`ScrollToLatest.isAway`, shared with the Mac), or when the message is the user's own.
  Scrolled away, the view stays put and a jump-to-latest button shows, as on the Mac (#284);
  tapping it goes to the newest message.

### Staying live without a cache
- Live events (`message.new`, `message.update`, `message.delete`, `reaction.update`) reach the
  open conversation through the list's one subscription, as on the Mac.
- iOS suspends the socket in the background, and core reconnects on return. Events sent
  meanwhile are lost. On the Mac the local cache fills that hole; iOS has none. So on iOS the
  open conversation re-reads its newest page on every `ready` it receives and when the app
  becomes active again. Re-reads that overlap join one fetch (the model already runs one head
  fetch at a time). This is the conversation's twin of the list's `rereadOnReconnect` (#272),
  and is off on the Mac, where a `ready` only retries a failed head, as today.
- Every newest-page fetch on iOS goes through the gap check below: the first load, a `ready`, a
  foreground re-read, and a `resync` (events dropped inside core). (Plan stage, approved by
  the owner 2026-10-09.)
- The anchor (the point a gap is closed back to) is the newest shown message at the moment the
  re-read is asked for: on `ready`, on `resync`, or on return to the foreground. It is not taken
  when the fetch runs, because live events can land in between: a live message or the user's
  own send would be newer than the gap and hide it. A `resync` takes its anchor at its trigger
  too, so it gets the same gap check as the others.
- A `ready` (a reconnect) clears the reaction order marks before its re-read: events may have
  been missed, and a restored server may number from lower values again (PROTOCOL.md §2,
  `reaction.update`). A foreground re-read with the socket still up keeps them: no event was
  missed, and the marks still order the ones in flight.
- The re-read page and the shown messages:
  - It reaches back to the anchor (ids are time-ordered): merged.
  - It does not (more than a page arrived while away): the model pages back with `before=`
    from the oldest fetched message until a page reaches the anchor, or the server returns
    none, up to 3 pages. Then everything fetched is merged. The page-back runs inside the same
    one-at-a-time head fetch as the re-read. A second `ready` meanwhile starts no fetch beside
    it; exactly one more newest-page fetch follows when the page-back ends, because that
    `ready` may know of newer messages. (Plan stage, approved by the owner 2026-10-09.)
  - Still no meeting point after 3 pages: the screen keeps what the re-read fetched, plus any
    shown messages newer than the newest fetched one (live arrivals and the user's own sends
    that landed while the pages ran). `atStart` and `olderFailed` are reset, and the view
    lands at the newest message, so the jump-to-latest button is gone. An older page
    (scrolling up) that was running when the replace happened is dropped when it lands (it
    belongs to the replaced history), and the request starts again by itself from the new
    oldest message, so the loader never waits on an answer that was dropped.
- Why not the server's `after=` forward sync: core's history call takes only `before`, and
  this needs no core change. A later core change can swap it in.
- Known limit, accepted for now: edits and deletes made while away to messages older than the
  re-read pages stay stale until the conversation is reopened.

### Read state
- The newest message is marked read on the server when the conversation loads, and each new
  one as it arrives while the app is active. Arrived while not active: marked when the app is
  active again (`readOwed`, the Mac's rule).
- A re-read (above) that brings newer messages marks the newest read when the app is active,
  and leaves the read owed when it is not. Today only a live event sets `readOwed`, and a
  re-read marks nothing; the re-read has to.
- Opening clears the row's mention badge. The plain unread count stays hidden (§3).

### Sending
- A multi-line text field (grows to about 6 lines) and a Send button. Return adds a new line;
  Send sends. This is the iPhone habit (Messages works this way).
- The box clears at once. The sent message is shown from the server's answer; the live echo of
  it merges into the same row, never a second one.
- Failure: the text comes back in the box, with the Mac's messages: a network failure says "It
  may not have been sent: check the conversation before sending again."; signed out says "You
  were signed out."; anything else "Couldn't send."
- Without the outbox the send goes straight to the server, with a `client_id` (§7, decision
  2): the composer's draft id, the same for the same text and quote after a failure. The
  server returns the stored message for a repeated id and never makes a duplicate (PROTOCOL.md
  §1, `POST /channels/{id}/messages`), so a send that timed out but arrived, sent again, shows
  once. Core's `send_message` and the binding gain an optional `client_id` for this; it lands
  before the iOS work. GNOME and KDE pass none in that change, so they behave as today; each
  passing its own draft id is a follow-up issue.
- Archived: no box; the Mac's note in its place, updated live from `channel.update`.

### What having no local cache costs
- Every opening fetches the newest page again, and every scroll back fetches again. One request
  per 50 messages. Fine on a phone; noted so nobody adds a cache for speed in this step.
- With no network, an opened conversation shows only the error. Nothing typed is kept.
- Read state and mention counts come from the server alone.

### Errors and limits
- No wire-contract change: the same routes, codes and events the Mac uses
  (`GET`/`POST /channels/{id}/messages`, `POST /channels/{id}/read`, `POST
  /channels/{id}/typing`, and the WebSocket events in PROTOCOL.md §2). Core now sends
  `client_id` on the direct send, a field the route already accepts; no route changes.
- A conversation's messages stay in memory only while it is open.

## 5. Sharing with the Mac app (input for the plan)

The rule for this step: shared code has no AppKit. Shared code that iOS compiles but never
calls is accepted, so the Mac's send path moves as it is instead of being split. How files
move is for the plan.

| Mac code | What happens | Notes |
|---|---|---|
| `ChannelsModel` (already shared) | used as is | `openChannel`, `timeline`, `channels`, `title`, `mentions`. Not `closed` (risks below) |
| `TimelineModel`, `ChatClient`, `ReactionRules`, `SaveModel` (`clients/macos/Brook/Chat/ChatModels.swift`) | move to shared | No AppKit. Needs `TypingState` (`Chat/TypingSearch.swift`, Foundation only), `AppActivity` (a seam both apps have), `PersonName` and `OfflineClient` (shared). With no local data its cache reads answer `local.unavailable` and it takes the network path. Gains the iOS re-read flag, off by default (§4) |
| `ComposerModel` (same file) | moves whole | Its file half stays; no iOS screen calls it. Its default `importer` names `PasteImport`, which stays on the Mac, so the Mac injects it and the shared default does not name it |
| `Chat/Staging.swift` | moves, except `PasteImport` | `PasteImport` reads `NSPasteboard` (AppKit) and stays Mac-only. `FileAccess`, `StagedFile`, `Staging`, `DropImport`, `StagingRefusal` move |
| `Chat/PendingModel.swift` | moves | Named by `ComposerModel` |
| `Chat/TypingSearch.swift` | moves whole | `TimelineModel` needs `TypingState`, the composer `TypingSender`. `SearchModel` and `SearchClient` come along: the whole file is Foundation only, so splitting it gains nothing |
| `FileRowModel`, `CacheFeed`, `NSWorkspaceBridge` | stay on the Mac | not needed |
| `ChatViews.swift` | stays on the Mac; pure helpers shared | `MessageRow.excerpt`, `MessageRow.time`, `AttachmentRow.icon`, `ScrollToLatest`. The views use AppKit and a desktop layout; iOS writes its own. `ComposerView.replyBanner` stays on the Mac: iOS has no reply yet, so nothing there would call it; it moves with the reply issue (plan stage, approved by the owner 2026-10-09) |

The Mac's tests that cover the moved code (`ChatModelTests`, `OfflineTimelineTests`,
`ReadWhenActiveTests`, `TypingSearchTests`, `ReactionTests`, `PendingModelTests`,
`ScrollToLatestTests`, and others) stay green. The plan decides where they run; they are not
duplicated.

Risks and notes for the plan:
- Lifecycle differs between the simulator and the device (#335). Navigation push and pop,
  `.task`, `.onDisappear` and scene phase are checked on the iPhone (Done 9).
- Keeping the reading position while an older page is added at the top, and keeping the box
  above the keyboard, are where plain SwiftUI scroll views most often misbehave. Both are in
  the device check (Done 2, 3).
- The move must leave the Mac's send path unchanged: the Mac's tests run before and after.
  The one exception (§7, decision 2): the Mac's direct send changes on purpose. Today the composer drops its draft id (`draft = nil`, ChatModels.swift:629) before
  the direct send; it must keep the id until the direct send succeeds, so a retry reuses it.
- `ChannelsModel.closed` is set on a removal and never cleared (ChannelsModel.swift, in
  `cacheRemoved`). A view that reacts to it changing misses a second removal of the same
  channel in one session (removed, re-added, removed again). Decided (owner, 2026-10-09): iOS
  does not use `closed`; it closes when the row leaves the list (§4), so it is not affected.
  `closed` stays as it is in the shared model, and the Mac's flaw gets its own issue.

## 6. Decisions

Taken in this spec (the owner may override):
- Send is a button; Return adds a new line. Scrolling dismisses the keyboard.
- Reactions, edits, deletes and reply quotes made elsewhere are shown, not made, on the phone.
- The open conversation re-reads on every `ready` and on foreground (iOS only), marks read
  after it, and fills a gap by paging back up to 3 pages before it replaces.
- No core change for the gap: `before=` paging, not `after=`.
- Shared code has no AppKit; the composer and its file code move whole.
- The typing signal goes out (it comes with the shared composer); others' typing is not shown.
- No drafts, no outbox, no retry queue.
- Plain unread counts: a separate issue.
- The decisions the owner took are in §7.

## 7. Decided by the owner (2026-10-09)

Each is the answer this spec recommended.

1. **Where the checks run.** The agent's checks run in the iOS simulator against the local
   stack on loopback (`clients/ios/README.md`, "Test against a local server"), seeding of more
   than 100 messages, archiving and removal included. Launch with `clients/ios/build.sh run`
   (simctl, no debugger attached), so suspension in the background is real. The real-iPhone
   check is the owner's, against chat.madalin.me (https), in a dedicated test channel, with the
   app launched from the Home Screen, not from Xcode. Why: a debugger keeps the app from being
   suspended, and the local stack listens on loopback only, which a phone cannot reach without
   a LAN bind and the insecure-http flag. Nothing is posted to real channels. At plan stage
   the owner added: in the agent's checks the second account acts through the server API, and
   no Mac app runs. Any Mac build shares the owner's bundle id, defaults and Keychain, so
   signing it in to the local stack would replace the owner's saved server and session. The
   Mac app as the other client, "shows on the Mac" included, is checked in the owner's iPhone
   check.
2. **The direct send is safe to repeat.** Core's `send_message` and the binding gain an
   optional `client_id`, and the composer passes its existing draft id. A small core change,
   done first, by the core implementer. The Mac's direct send gains the same safety. At plan
   stage the owner added: GNOME and KDE pass none in the core commit (one `None` each, forced by
   the new signature); each passing its own draft id is a follow-up issue.
3. **New messages while reading history.** Follow only at the bottom or for the user's own
   message, plus the jump-to-latest button, sharing `ScrollToLatest.isAway` with the Mac.

No open questions remain.
