# Mac: sidebar labels and last-used order (#219): plan

Each step builds and passes `cargo test -p brook-core -p brook-ffi`; steps 3 to 5 also pass
`clients/macos/build.sh test` (the owner's Mac only). One PR; each new test is watched failing under the
mutant named beside it.

1. **Core rules** (`core/src/sidebar.rs`, `sidebar_tests.rs`, re-exported in `lib.rs`, with a doctest as the
   external-crate check). Mutants: ignoring `show_usernames`; no `trim`; no `#`; first member instead of
   the other (fixture: empty `me`, expect the joined labels); no section key; ascending order; none-first
   ordering; no opened tie-break; sorting by the shown label instead of the name key (fixture: preference on,
   handle order differs from name order); id tie-break dropped; `>=` in `activity_moves`;
   `conversation_label` returning empty for no members.
2. **Core cache read** (`cache.rs`, `client_offline.rs`): `CachedChannel` and `Channel` gain
   `last_message_id` (`#[serde(skip)]` on `Channel`), from a subquery over the cache's messages; update the
   existing `CachedChannel` literal in `client_offline.rs`. Tests: tombstone counts; an acknowledged outbox
   message counts without any socket echo (a behaviour test guarding the cache/outbox split, no mutant of the
   query can fail it). Mutants: `min`; a deleted filter.
3. **Bindings**: `FfiCachedChannel.last_message_id`; free functions `person_label`, `conversation_label`,
   `sidebar_order` (taking `FfiSidebarEntry`), `activity_moves`. Rebuild the xcframework. Update every
   `FfiCachedChannel` constructor in the Mac fakes (`OfflineFakes`, `ChannelsOfflineTests`). No new
   `OfflineClient` method, so `FakeChat` needs no new conformance. Test: record mapping in
   `offline_tests.rs`.
4. **Mac labels and preference**: `Settings.showUsernamesKey`; `SwiftUI.Settings { … }` scene with the
   toggle and caption; `ChannelRow.label` from `conversationLabel`, built by one row factory used by every place a row is made
   (load, `channel.update`, reveal) and recomputed on toggle; a `channel.update` rename shows at once (test);
   `ChannelsModel.title` returns it (the Swift `ChannelTitle` is removed; its tests move to the model
   level). Tests (spec 1 to 3). Mutants: default true; `title` ignoring the preference.
5. **Mac order**: `ChannelsModel` gets `activity`, `opened` (persisted per account in `UserDefaults`, written on
   the main actor; the counter starts at `max(saved) + 1`; nothing is persisted while `me` is unknown), and `resort()` calling `sidebarOrder`. Rules: `reloadList` applies
   `max(current, cache)` after its generation check and re-sorts; `message.new` records the key (also for
   unknown channels) and re-sorts when `activityMoves` against the `sorted` key, which is set at each re-sort; `cacheChannelsChanged` only moves keys forward and
   re-sorts only if a conversation other than the open one, or one opened whose back-fill has not been seen, moved; `openChannel.didSet` raises the rank and
   never re-sorts. New `ChannelOrderTests`, with fixtures: out-of-order `cachedChannels`; a click before a
   notice; names whose display order differs from handle order; local data off, live message, then reload;
   the cache notice before and after its live message (both orders re-sort once), the back-fill notice for the open channel, and for A when B was opened right after A; saved ranks, a new model, one click: that channel ranks first; a message for an unknown channel. Mutants: no sort in
   `reloadList`; no re-sort on `message.new`; a re-sort in `didSet`; a re-sort on every notice; cache key
   overwriting the live one (no `max`); no handling of the open channel in a notice; comparing against `activity` instead of `sorted`.
6. **Run and show**: `cargo test -p brook-core -p brook-ffi`, `cargo clippy`, `clients/macos/build.sh test`,
   each mutant output pasted. Against a test server: a second account posts and the conversation moves up;
   restart; toggle the preference. The PR states what could not be seen (GTK unchanged; the live run needs
   the owner's Mac).

## If it stops halfway

- After 1 or 2: nothing visible. After 3: the Mac behaves as today. After 4: labels and the preference
  work; the order is still the server's. No step writes anything older code cannot read.

## Where it fails

- A missed xcframework rebuild breaks the Swift fakes' constructors; `build.sh test` shows it.
- `sidebarOrder` crosses the FFI once per re-sort (hundreds of rows at most), never from `body`.
