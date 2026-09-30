# Mac: reactions, mention highlight, typing and search: spec

Status: spec. Review dial: **Standard** (bindings and UI on existing core calls; no auth, storage or
wire change). Two PRs: **A** reactions and the mention highlight, **B** typing and search. GTK is the
reference (`clients/gnome/src/chat.rs`); the Mac has none of these today, so reactions from other
clients never show and it can't react.

## Done means

### A. Reactions and the mention highlight
1. **Bindings:** `FfiReaction {emoji, count, me}` and `FfiMessage.reactions`; the event
   `FfiServerEvent.reactionUpdate {channelId, messageId, emoji, userId, added}`; and
   `toggleReaction(channelId, messageId, emoji) -> [FfiReaction]` (the message's full summary from
   your side). Each mapping is tested under a mutant.
2. **Chips under a message's text:** "👍 3", filled when you're one of them, one per emoji in the
   order the server gives. A click toggles your reaction. None on a deleted message.
3. **React** in the message's context menu: the quick set 👍 ❤ 😂 🎉 👀 🙏 (as GTK). Toggling calls
   `toggleReaction`, and the answer **replaces** that message's reactions.
4. **Live events** (`reactionUpdate`) for a message in the open channel set that emoji's chip from
   the event's **new total** (`count`; 0 removes the chip; an emoji not there yet gets a chip), and
   when the event is yours (`userId` is you: another device, or the echo of your own toggle) it
   sets `me` to `added`. The event is absolute, so applying it twice, or before or after the
   toggle's own answer, ends the same. A message that isn't loaded is ignored (history brings its
   reactions when it loads). With local data the cache re-syncs on `reaction.update` and the
   timeline re-reads the row, so its reactions arrive whole and replace these.
5. **A message that mentions you** (`NotificationPlanner.mentions`: someone else's, not deleted, you
   or everyone) gets a light accent background in the timeline.
6. **Tests** (models against fakes, each under a mutant): the event rules above (new chip, a count
   update, removal at 0, yours sets `me`, applying one twice changes nothing, an unknown message
   and another channel ignored); toggle replaces
   from the answer; a failed toggle shows its text and changes nothing; the tint's rule; deleted
   messages show none.

### B. Typing and search
7. **Bindings:** `FfiServerEvent.typing {channelId, userId, displayName}`; `sendTyping(channelId)`;
   `searchMessages(query) -> [FfiMessage]`.
8. **Typing, sent:** while you type in the composer, at most one `sendTyping` per 3 seconds, only
   for a non-empty draft; a failure is ignored.
9. **Typing, shown:** a line above the composer for someone else typing in the open channel:
   "Ann is typing…", "Ann and Bob are typing…", "Several people are typing…" (three or more); it
   clears 4 seconds after the last event from them, and when a message from them arrives.
10. **Search** (online only, like GTK): a search field in the sidebar; Return searches message
    bodies across your channels (newest first); results show the channel, the author and an excerpt;
    choosing one opens that channel. Empty query clears. Offline, or a failure: "Search needs a
    connection."; no results: "No messages found."
11. **Tests:** the typing throttle (injected clock), the names line for one, two and many, the
    4 s expiry and the clear-on-message, your own typing ignored, other channels ignored; search:
    the call, the states (results, none, failure), opening a result.

## Not doing

- Picking any emoji (only the quick set), custom emoji, who reacted (a hover list).
- Searching the local cache offline (server search only, as GTK).
- Scrolling to the found message (it opens the channel).
- Typing for threads or per-message.

## Where it fails

- **An event before the message loads:** ignored; the loaded row carries the summary.
- **My own toggle's echo before or after the answer:** both orders end the same, because the
  event's count is absolute (§4).
- **A typing event after the message it announced:** the message clears it, so a stale "typing…"
  never outlives the message.

## Review round 1 (vibe; Standard)

Spec: no findings.

Found while implementing: core's reaction event carries the emoji's **new total** (`count`), which
makes the live rule absolute (set the chip's count; `userId` is you ⇒ set `me`), replacing the
first draft's ±1. Spec §4 is updated.

Implementation review (vibe), PR A:
- **Taken:** a failed toggle's text is now shown above the composer (it was set and never read).
- **Rebutted:** "the tint test doesn't cover your own messages". The tint is
  `NotificationPlanner.mentions`, whose own and deleted cases `MentionRuleTests` (#198) pins; the
  tint test is renamed so it doesn't promise more than it checks.

Measured (PR A): 12 mutants and 4 binding mutants, each caught (one as a hang past 180 s; two
rewritten because their first form didn't compile). 279 Mac tests, 35 binding tests.
Not unit-tested: the chips and the context menu (SwiftUI).

## PR A review, round 1 (codex, Claude review (Opus); Mac-only rule)

Taken (both reviewers found the same two things):
- **A toggle's answer could overwrite newer counts,** and two toggles on one message could
  drop each other's reaction (the newer whole-list answer arriving before the older). Now one
  toggle per message at a time, and the answer is used only if no reaction event for that
  message arrived while it was in flight (the events carry the newer counts, your own echo
  included). The first draft's claim that order never matters was wrong and is corrected in the
  code's comment: the same event applied twice is harmless, but two *different* events for one
  emoji can arrive out of order (the server doesn't order them), which lasts until the next
  event, a re-read or the cache's sync. Ordering by `seq` needs core to pass it through; noted
  for later, and asked of the server side.
- **The reaction error now goes by itself** after 5 seconds.
- §10 says the search field is in the sidebar (it was written "toolbar").

Measured: 5 more mutants, each caught. 282 Mac tests.
