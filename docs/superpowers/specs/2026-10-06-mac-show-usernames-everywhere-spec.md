# Mac: every person follows Show usernames (#238, Mac half)

Review dial: **Standard** (local presentation only; no storage, auth, server or contract change; reversible by toggling the setting).

## Design
- **One rule, one wrapper.** New `clients/macos/Brook/PersonName.swift`: `enum PersonName` with `static let unknown = "Someone"`;
  `label(_ name: String?, handle: String?, showUsernames: Bool) -> String` (trimmed handle empty: the trimmed name, or `unknown` when
  blank too: the guard for core's `"@"` for an empty handle, as GTK's `author_text`; otherwise `BrookCore.personLabel(displayName:handle:showUsernames:)`);
  `other(...) -> String?` (the form not shown, for a second line: `@handle` when names show, the trimmed display name when handles show; nil when blank,
  no handle, or equal to the label); `both(...) -> String` (`"label (other)"` or the label alone). Also `extension EnvironmentValues { @Entry var showUsernames = false }`
  and `struct FollowsShowUsernames: ViewModifier` (holds `@AppStorage(Settings.showUsernamesKey)`, sets `.environment(\.showUsernames, on)`), applied as the
  **outermost** modifier of `SignedInView.body` (so sheets, popovers and confirmation dialogs are covered) and of `CallWindow` (a separate `Window` scene: an
  environment set in SignedInView never reaches it). The existing `@AppStorage` + `onChange` push into `ChannelsModel` (SignedInView ~70, ~207) stays.
- **Models take the flag as a parameter with no default**, so the compiler finds every call site. Views read `@Environment(\.showUsernames)` and pass it in.
- **People are stored as (name, handle), never as a finished string**, so a toggle relabels what is on screen without a reload.
- **Member list:** primary line = label (+ " (you)"); secondary line = `PersonName.other`. Order: you first, then by the label shown (case-insensitive), then by id
  (toggling reorders; the list is purely alphabetical, unlike the sidebar).
- **Data the surface lacks:** the typing event and `FfiParticipant` carry `display_name` only. Typing gets the handle from the open channel's members, then from authors
  seen in the timeline, else shows the name. Call tiles get the handle from a userId -> handle map of the channel's members at join time; a participant not in it shows
  their name. No core or FFI change.

## Surfaces (data available -> what it shows)
| # | Surface | where | Shows |
|---|---|---|---|
| 1 | Message header | ChatModels.swift `authorName` (~149), `refreshAuthors` (~206), ChatViews MessageRow | label; a `Users` refresh replaces both fields |
| 2 | Reply banner | ChatViews.swift ~389 | "Replying to <label>"; "Replying to a message" when both blank |
| 3 | Member rows | MembershipViews.swift ~45-46, MembershipModel `rows` ~72-77 | label (+ " (you)"), `other` below, sorted by label |
| 4 | Offer / Remove titles | MembershipViews.swift ~72, ~82 | "Offer <label> ownership of <title>?", "Remove <label> from <title>?" |
| 5 | "X offered you" | MembershipModel ~231-233, SignedInView ~353, MembershipViews ~100 | label; "Someone" once gone |
| 6 | Notification body | Notifications.swift ~22-26, posted from ChannelsModel ~300 (already holds `showUsernames`) | "<label>: ...", "<label> mentioned you: ...", "<label> sent a file" |
| 7 | Search result author | TypingSearch.swift ~89, SearchViews ~28 | label, computed when drawn |
| 8 | Typing line | TypingSearch.swift TypingState, ChatModels ~316, ChatViews ~80 | label; the name if no handle is known |
| 9 | Call tiles, "X's screen" | CallModel.swift ~132-150, CallViews ~54/67 | label; the name if no handle; "You" stays |
| 10 | "Signed in as" | SignedInView ~173 | "Signed in as <label>" |
| 11 | Admin reset-password picker and done text | AccountViews ~64, AccountModels ~195 | picker `both` ("Ann (@ann)" / "@ann (Ann)"); done "<label>'s password is set..." |
| 12 | Admin reset-2FA picker and done text | TwoFactorViews ~185, TwoFactorModels ~255 | same as 11 |
| 13 | Settings caption | SettingsView ~11 | see Done 13 |
Already following the setting: the sidebar and DM titles (`conversationLabel`) and the window title. No person shown (out of scope): the quote line, `PendingRow`, the conversation
sheets (handle text fields), the add-user done text (echoes the typed handle).

## Done means
1. `PersonName.label`: ("Ann","ann",off) "Ann"; on "@ann"; (" Ann ","ann",off) "Ann"; ("  ","ann",off) "@ann"; (nil,"ann",off) "@ann"; ("Ann",nil or "",on) "Ann";
   (nil,nil) and ("","") either setting "Someone". `other` and `both` have their own tables. Tests: `PersonNameTests.testLabelTable`, `testOtherTable`, `testBothTable`.
2. A message from Bob/@bob reads "Bob" off, "@bob" on; after a `Users` notice renames him Robert: "Robert" / "@bob"; no handle: the name; neither: "Someone".
   Test: `OfflineTimelineTests.testAuthorNameFollowsShowUsernames` (replaces the assertions near :187).
3. Reply banner "Replying to Bob" / "Replying to @bob"; neither: "Replying to a message". Test: `ChatModelTests.testReplyBannerFollowsShowUsernames`.
4. Members Ann/@zed and Bob/@amy plus you: off [you, Ann, Bob] with second lines "@zed", "@amy"; on [you, @amy, @zed] with second lines "Bob", "Ann"; a blank display name with
   the setting on has no second line. Test: `MembershipProfileTests.testRowsSortAndSecondLineFollowShowUsernames`.
5. `MembersView.offerTitle` / `removeTitle` (static): "Offer @bob ownership of #general?" on. Test: `MembershipProfileTests.testConfirmTitlesFollowShowUsernames`.
6. `OfferAnswerModel.offererName(_:members:showUsernames:)`: "Owner Ann" / "@own"; "Someone" when gone. Test: `OwnerOfferTests.testOffererFollowsShowUsernames`.
7. `NotificationPlanner.body(_:me:showUsernames:)`: "@bo: hello", "@bo mentioned you: look", "@bo sent a file" on. Test: `NotificationsTests.testBodyFollowsShowUsernames`.
8. With `ChannelsModel.showUsernames = true` the next posted notification body starts "@bo". Test: `NotificationsTests.testPostedBodyUsesModelsPreference` (fake notifier).
9. `SearchHit.author(showUsernames:)`: "Ann" / "@ann". Test: `TypingSearchTests.testSearchHitAuthorFollowsShowUsernames`.
10. Typing: with Bob/@bob among the members "Bob is typing..." off, "@bob is typing..." on; a typist not among the members whose message is in the timeline: the message's handle; known by
    neither: "Bob is typing..." either way. Tests: `TypingSearchTests.testTypingLineUsesMemberHandle`, `testTypingLineFallsBackToName`.
11. `CallModel.tiles(showUsernames:)`: "@linux" and "@linux's screen" with the handle known, "Linux" unknown, "You" for yourself either way. Tests: `CallModelTests.testTilesFollowShowUsernames`, `testTileWithoutHandleShowsName`.
12. `AdminResetModel.submit(showUsernames: true)` -> "@bob's password is set. They're signed out everywhere."; the same for `AdminTotpResetModel`. Tests: `AdminResetModelTests.testDoneNamesByPreference`, `AdminTotpResetModelTests.testDoneNamesByPreference`.
13. The Settings caption is exposed as `SettingsView.showUsernamesCaption` (static), asserted in `SettingsTests.testShowUsernamesCaption`: "Shows people as @username instead of their display name, everywhere they appear."
14. `clients/macos/check-person-names.sh`, run by `build.sh test` before Xcode, fails if any `Brook/**/*.swift` line outside `PersonName.swift` reads `.displayName` or `authorDisplayName` (excluded: `displayName(` which is
    `DropImport`, and lines marked `// raw name: <why>`: storing (name, handle) pairs, `ProfileModel` editing your own name, `updateProfile(displayName:)`). Proof: watched failing on a scratch line that reintroduces `m.displayName`.

## Not doing
GTK (done in #253) and any core or FFI change (no `author_handle` on `FfiReplyExcerpt`, no handle on `FfiParticipant`, no moving the empty-handle guard into core); adding the author to the Mac quote line (a new
feature); relabelling text already handed off when the setting is toggled (notifications posted, an admin done text shown, the call window's title, a DM label taken at join, an open call's handle map);
fixing cached rows whose author was never refreshed (they keep the stored name and handle); changing the sidebar order rule.

## Where it fails
No handle (bot or deleted author: `author_handle: None`): the name, then "Someone" (by design). Blank display name: `@handle` even with the setting off (core's rule). Disabled user: shown as stored.
`Users` notice: `refreshAuthors` replaces both fields. A typist or call participant not in the members list: their display name with the setting on (harmless inconsistency). An environment that does not reach
a presented view reads the default (off): defence = outermost placement + the manual checklist. Toggling during an open sheet or popover: environment text relabels at once; the member list re-sorts under the cursor.
Cost: one synchronous FFI string call per label per render (about 50 rows plus the 1 s typing tick): expected negligible, not measured.

## Decisions taken (owner may override)
Toggling reorders the member list (sorted by the label shown); the Mac keeps "Someone" for a person with neither name nor handle (GTK says "Unknown"); the admin pickers show "Ann (@ann)" / "@ann (Ann)".
