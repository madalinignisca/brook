# Mac: sidebar labels and last-used order (#219): spec

Status: spec, closed after review round 2 (codex, Claude). Review dial: **Standard**
for the spec, plan and diff (two external reviewers plus the Claude review, as for any Mac change).
Round 1 first set it to Heavy for a cache format bump; that bump was dropped (see Design), so no storage
or auth changes remain. The pure rules live in core so GTK can reuse them; the GTK UI is a follow-up.

## Design

- **No cache change.** The first draft stored an "opened" rank in the cache and bumped its format. Review
  found that a format bump deletes `cache.db` including `files.pinned` and each file's key
  (`core/src/store.rs`), which would silently lose Keep available offline. The last-activity key is a
  read-only query instead, and the opened rank is a per-device client preference.
- **Core, new `core/src/sidebar.rs`, public and re-exported at the crate root** (GTK depends on
  `brook-core`, not on the Apple bindings; a doctest uses it from outside the module):
  - `person_label(display_name, handle, show_usernames) -> String`: `@handle` when the preference is on;
    else the trimmed display name; if that is empty, `@handle`.
  - `conversation_label(kind, name, members, me, show_usernames) -> String`: a non-empty name is `#name`;
    a DM is the other member's `person_label` (an empty or unknown `me` picks no "other": the members'
    labels are joined instead); otherwise the members' labels joined by commas; and
    `"Direct message"` when nothing is known. Never empty, so the sort key is always defined.
  - `sidebar_order(entries) -> Vec<String>` (ids). Channels (`kind != "dm"`) before DMs. Within a section:
    newest message id descending, none last; then opened rank descending, none last; then the **name
    key** (lowercase `conversation_label` with `show_usernames = false`, so the preference can never
    change the order); then id. `SidebarEntry` carries that key as `sort_key`; core offers `sort_key(kind, name, members, me)` so no
  client computes it with the preference on.
  - `activity_moves(current: Option<&str>, message_id: &str) -> bool`: strictly newer.
- **Last activity:** `cached_channels` gains `last_message_id`, `max(m.id)` over the channel's cached
  messages (tombstones included, so a deletion never lowers it). Message ids are UUIDv7 text, so they sort
  by time. A read-only query: no schema or format change.
- **Opened rank:** an in-memory counter in the Mac's model (it starts at `max(saved ranks) + 1` after a launch), persisted as a `[channelId: Int]` dictionary in
  `UserDefaults` under a key per account. Written synchronously on the main actor (no detached writes, so
  no ordering race). Entries for channels no longer in the list are dropped when the list loads.
- **Model state is separate from rows.** `ChannelsModel` keeps `activity: [id: messageId]` and
  `opened: [id: Int]` as dictionaries; rows get their keys from them. A reload, a `channel.update` that
  replaces a row, or a late cache read can only move a key forward (`max`). A message for a channel that is
  not in the list yet is recorded in `activity` and applies when the row arrives (the reload the unknown
  `channel.update` starts).
- **Two keys per conversation:** `activity` (the newest id seen, from the cache or live) and `sorted` (the
  value at the last re-sort). `activity_moves` compares a live message against `sorted`, not `activity`, so a
  cache notice that already advanced `activity` for the open conversation cannot swallow its later
  `message.new` (either order of arrival re-sorts once the live message is heard).
- **When the list re-sorts:** on a list load; on a `message.new` that moves the key (`activity_moves`);
  and on a cache `Channels` notice where a key moved forward for a conversation that is neither the open
  one nor one opened whose back-fill has not been seen yet (its history fetch raises its own key; a channel
  leaves that set when a notice has moved its key, or when a live message for it arrives; a re-sort does
  not clear it, because the fetch can finish after any number of re-sorts).
  A click never re-sorts, and neither does the history back-fill that opening starts (it raises the open
  conversation's own key). A re-sort applies the whole rule, so earlier clicks can show then.
- **Labels in the row:** the model stores each row's label, set by one row factory used wherever a row is
  built (list load, `channel.update`, reveal) and recomputed when the preference toggles, so `body` never
  calls into core per row and a rename shows at once. Notification titles use the same labels.
- **Preference:** `UserDefaults` key `ShowUsernames`, off by default, a Toggle in a Settings window
  (`SwiftUI.Settings`, named in full: the app has its own `Settings` struct).

## Done means

1. A channel reads `#general` in the sidebar, the window title and notifications. Tests: core
   `channel_label_is_hash_name`; `ChannelLabelTests`.
2. A DM reads the other person's display name; with Show usernames on, `@bob`. A blank or whitespace
   display name reads `@handle` either way. Core tests `dm_label_follows_the_preference`,
   `blank_name_falls_back_to_at_handle`.
3. "Show usernames" is off on a fresh install, relabels the sidebar at once and moves no row (fixture:
   display-name order differs from handle order).
4. Every channel row is above every DM row; within a section the order is the rule above. Core tests:
   section split, newest first with none last, tie by opened rank then label then id (mutants in the plan).
5. A live `message.new` re-sorts by the updated key (it moves to the top of its section unless another
   conversation is newer). Badges are unchanged. A message not newer than its key moves nothing.
6. A click moves no row; neither does the back-fill. With local data off, keys learned live survive a list
   reload (fixture: live message, then `reloadList`, order kept).
7. With local data on, a restart recomputes the same rule from the cache and the saved ranks, so the order
   is the one that rule gives (not a replay of what was shown).
8. A message for an unknown channel is kept and applies when its row arrives.
9. Core: `cached_channels_carry_the_newest_message_id_including_tombstones`; an acknowledged outbox message
   counts (the pending ones live in the separate outbox database, so a queued message never appears here); the pure rules are usable from
   an external crate (doctest).

## Not doing

- The GTK UI (its own plan, same core API). #219 stays open until it lands.
- Labels in message headers and member lists: they keep display names while Show usernames is on, until a
  follow-up on #219.
- A server `last_message_at`; the same order across devices; section headers; archived grouping; pinning;
  locale-aware collation (core compares lowercase); KDE.

## Where it fails

- **Fresh sign-in, a rebuilt cache or a 410 reset:** keys are empty, so the order is by opened rank, then
  A to Z, until messages arrive. Local data off: same at every launch (ranks persist, keys do not).
- **A re-sort can move a row a click raised earlier** (the whole rule applies); a click itself never does.
- **A conversation opened this session whose news arrives only by the cache's catch-up sync** (no live
  message) has that first notice treated as its back-fill, so it moves at the next re-sort from any cause,
  not at once.
- **Two messages in the same millisecond:** the random part of the UUIDv7 decides.
- **Message ids are compared as text:** the server issues canonical lowercase UUIDv7 strings.
- **No server calls are added**, so there are no refusal codes.

## Review record

Round 1 (codex, Claude): the cache format bump (lost pinned files), in-memory keys lost on reload and
replace, the unknown-channel path, contradictory restart criteria, the click jump through the history
back-fill, the Settings name clash, label cost: all taken (design section). Round 2: the tie-break now
ignores the preference; labels come from one row factory; the opened counter starts above the saved
ranks; the pending-row mutant is gone; empty `me` joins labels; a back-fill finishing after a switch is
suppressed by the opened-since-last-sort set; the cache-versus-live arrival order is covered by the
`sorted` key. Rebutted: none. Closed (no round 3). Found in review of the diff and fixed: the opened set was cleared by every
re-sort, so a late back-fill moved a row (now consumed by its notice); an empty offline list erased the
saved ranks (pruning only on a network list).
