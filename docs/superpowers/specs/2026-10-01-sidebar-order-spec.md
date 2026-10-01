# Mac: sidebar labels and last-used order (#219): spec

Status: spec, closed after review round 1 (codex, Claude); round 2 pending. Review dial: **Standard**
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
    a DM is the other member's `person_label`; otherwise the members' labels joined by commas; and
    `"Direct message"` when nothing is known. Never empty, so the sort key is always defined.
  - `sidebar_order(entries) -> Vec<String>` (ids). Channels (`kind != "dm"`) before DMs. Within a section:
    newest message id descending, none last; then opened rank descending, none last; then label
    (lowercase); then id.
  - `activity_moves(current: Option<&str>, message_id: &str) -> bool`: strictly newer.
- **Last activity:** `cached_channels` gains `last_message_id`, `max(m.id)` over the channel's cached
  messages (tombstones included, so a deletion never lowers it). Message ids are UUIDv7 text, so they sort
  by time. A read-only query: no schema or format change.
- **Opened rank:** an in-memory counter in the Mac's model, persisted as a `[channelId: Int]` dictionary in
  `UserDefaults` under a key per account. Written synchronously on the main actor (no detached writes, so
  no ordering race). Entries for channels no longer in the list are dropped when the list loads.
- **Model state is separate from rows.** `ChannelsModel` keeps `activity: [id: messageId]` and
  `opened: [id: Int]` as dictionaries; rows get their keys from them. A reload, a `channel.update` that
  replaces a row, or a late cache read can only move a key forward (`max`). A message for a channel that is
  not in the list yet is recorded in `activity` and applies when the row arrives (the reload the unknown
  `channel.update` starts).
- **When the list re-sorts:** on a list load; on a `message.new` that moves the key (`activity_moves`);
  and on a cache `Channels` notice where a key moved forward for a conversation that is *not* the open one.
  A click never re-sorts, and neither does the history back-fill that opening starts (it raises the open
  conversation's own key). A re-sort applies the whole rule, so earlier clicks can show then.
- **Labels in the row:** the model stores each row's label at load and when the preference toggles, so
  `body` never calls into core per row. Notification titles use the same labels.
- **Preference:** `UserDefaults` key `ShowUsernames`, off by default, a Toggle in a Settings window
  (`SwiftUI.Settings`, named in full: the app has its own `Settings` struct).

## Done means

1. A channel reads `#general` in the sidebar, the window title and notifications. Tests: core
   `channel_label_is_hash_name`; `ChannelTitleTests`.
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
9. Core: `cached_channels_carry_the_newest_message_id_including_tombstones`; acknowledged outbox messages
   count and pending ones do not (`last_message_id` ignores pending rows); the pure rules are usable from
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
- **Two messages in the same millisecond:** the random part of the UUIDv7 decides.
- **Message ids are compared as text:** the server issues canonical lowercase UUIDv7 strings.
- **No server calls are added**, so there are no refusal codes.
