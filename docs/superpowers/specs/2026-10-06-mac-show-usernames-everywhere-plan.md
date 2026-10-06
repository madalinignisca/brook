# Plan: every person follows Show usernames on the Mac (#238)

Each step builds and passes `clients/macos/build.sh test` on its own; each new test is watched failing against the step's mutant before it is kept.
1. **Wrapper.** `PersonName.swift` (`label`, `other`, `both`, `@Entry showUsernames`, `FollowsShowUsernames`) and `BrookTests/PersonNameTests.swift` (Done 1). Unused yet.
2. **Headers and "Signed in as".** In `TimelineModel` replace `authorNames: [String: String]` with `authors: [String: (name: String, handle: String)]` filled by `refreshAuthors` from `FfiMember`;
   `authorName(_:showUsernames:)` with no default; `ChatView` reads the environment; `.modifier(FollowsShowUsernames())` last in `SignedInView.body`; SignedInView ~173 uses `PersonName.label`. Update OfflineTimelineTests ~187 and SessionStoreOfflineTests ~553. (Done 2)
3. **Reply banner.** `static func replyBanner(_ m: FfiMessage, showUsernames:) -> String` on `ComposerView`, used at ChatViews ~389. (Done 3)
4. **Members.** `ChannelPowers.rows(showUsernames:)` with the new sort; MembersView second line uses `PersonName.other`; `offerTitle` / `removeTitle` statics; `OfferAnswerModel` stores `offeredBy: FfiMember?`, `offererName(_:members:showUsernames:)`;
   the sheet reads the environment; `syncAnswering` passes the member. (Done 4-6)
5. **Notifications.** `body(_:me:showUsernames:)`; ChannelsModel ~300 passes `showUsernames`. (Done 7-8)
6. **Search and typing.** `SearchHit` stores `authorName?`/`authorHandle?` and `author(showUsernames:)`; `TypingState` stores the userId with the name, `line(now:label:)` takes a `(userId, name) -> String`;
   `TimelineModel` gains `members: @MainActor () -> [FfiMember] = { [] }` (a data source, not the flag, so a default is acceptable) and `typingLine(now:showUsernames:)`; SignedInView ~391 passes the members lookup. (Done 9-10)
7. **Calls.** `CallModel.init` gains `handles: [String: String]`; `tiles(showUsernames:)`; `CallCenter.join` gains `handles:` (SignedInView ~124 passes the map from `channel.members`);
   `CallWindow` applies `FollowsShowUsernames`; `CallView` reads the environment. (Done 11)
8. **Admin.** `submit(showUsernames:)` on `AdminResetModel` and `AdminTotpResetModel`; both pickers use `PersonName.both`. (Done 12)
9. **Guard and caption, last.** `check-person-names.sh`, called from `build.sh test` next to `test-install.sh`, with `// raw name:` markers where needed; the caption and `showUsernamesCaption` with its test. (Done 13-14)

Mutants that must each turn a named test red: `label` passes `false` to core; `label` drops the blank-handle branch (returns "@"); "Someone" becomes ""; `refreshAuthors` keeps the message's handle; `rows` sorts by `displayName`;
`other` always returns "@handle"; `NotificationPlanner.body` ignores the flag; ChannelsModel ~300 passes literal `false`; `typingLine` ignores the `members` lookup; the screen tile uses `p.displayName`; admin `done` uses `displayName`; `offererName` ignores the flag.

Untested wiring (owner checks in the running app, setting on): the modifier's placement in SignedInView and CallWindow; the environment reaching the members popover, its two confirmation dialogs, the owner sheet, both admin sheets and the search list;
ChatView, SearchViews and ComposerView passing the environment; SignedInView passing `members:` and `handles:`.

If it stops halfway: any prefix of 1-8 leaves some surfaces following the setting and others not (today's state with fewer gaps); no data, cache or setting is touched, so it is safe to ship or revert at any step. The caption changes only in step 9.
Where it fails: changed signatures break a lot of test code at once (`tiles` and `rows` become functions; `submit` gains a parameter: about 62 call sites): each step updates its own tests in the same commit; the guard is a heuristic (it misses a name read through another property): the per-surface tests cover the known surfaces; a full disk shows up as a link-step "test failure" (errno 28): clear `target/debug`.
