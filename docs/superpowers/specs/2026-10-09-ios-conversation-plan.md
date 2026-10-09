# Plan: iOS opens a conversation, reads it, sends to it (#337)

Spec: [2026-10-09-ios-conversation-spec.md](2026-10-09-ios-conversation-spec.md). Review:
`brook-reviewer` for every step. Step 1 changes core, so its review also covers the server and
protocol side (step 1 says what to check). No step touches authentication or authorization, so
`auth-reviewer` is not needed.

## 1. Approach

The work goes in this order, and each step is one pull request that leaves `main` green:

1. Core and the Apple binding learn an optional `client_id` on the direct send, and the composer
   passes its draft id (spec §7, decision 2).
2. The conversation models move from `clients/macos` to `clients/apple-shared`, with no change in
   behavior. The Mac's tests prove it.
3. The shared `TimelineModel` gains the iOS re-read rule, behind a flag that is off on the Mac.
4. The iOS conversation screen: open, read, scroll back, stay live, close on removal.
5. The iOS message box: send, errors, archived note, the keyboard.
6. The owner's check on a real iPhone.

There is no separate docs step: each step's pull request carries the doc edits that step makes
necessary (READMEs, PROTOCOL.md, code comments, and the spec's "as built" notes), written by
`brook-docs-writer` (or `brook-spec-writer` for the spec) as a commit in that pull request. A
doc that no longer matches the code is a bug, so none waits for a later step.

The core change comes first because it is the only change outside Swift, and the only one that
another client (GNOME, KDE) builds against. The move comes before any iOS code, so the review of
the move only has to answer one question: did the Mac change?

**Sharing: move the files, do not copy them.** `clients/macos/project.yml` and
`clients/ios/project.yml` both list the folder `../apple-shared/Brook` (and `../apple-shared/BrookTests`
for tests). A file put under that folder is compiled by both apps the next time `build.sh` runs
`xcodegen`. So no `project.yml` changes. Files move with `git mv`, so `git log --follow` keeps
their history.

**The one AppKit dependency: an importer the Mac sets.** `ComposerModel.importer` is already a
settable property (the drop tests set it). Its default names `PasteImport`, which reads
`NSPasteboard` and `NSBitmapImageRep` and cannot build on iOS. The shared default becomes plain
`DropImport.copy`, and the Mac sets its own importer (today's closure, word for word) where it
builds its one composer. Alternatives considered:
- *`#if canImport(AppKit)` in the shared file.* The seam rule (`clients/ios/README.md`) forbids
  platform branches in shared files.
- *A seam type each app defines (like `AppActivity`).* iOS would have to define an importer for
  a drop and paste screen it does not have. The property already exists, so no new concept is
  needed.

**The direct send's `client_id`: one optional argument.** Core's `send_message` and the binding's
`send_message` gain `client_id: Option<...>`. Core puts it in the JSON body only when it is given.
The route already accepts it (PROTOCOL.md §1, `POST /channels/{id}/messages`). Alternatives
considered:
- *A second core function* (`send_message_with_id`): two ways to do one thing.
- *Always generating an id in core*: core cannot know that two calls are the same message. The
  composer can, through its draft.

**The iOS re-read: a flag on `TimelineModel`, inside the head fetch that already exists.**
`TimelineModel` already runs one newest-page fetch at a time (`headDrain`), and a request that
arrives during one queues exactly one more. With the new init flag `rereadOnReady: true`, that
fetch also fills a gap: it pages back with `before=` up to 3 times until a page meets the shown
messages, and replaces them if none does. Because the page-back runs inside the same drain, a
second `ready` during it joins the drain instead of starting another fetch beside it. The point
the pages must reach back to (the gap anchor) is taken when the re-read is asked for, not when
the fetch runs, so a live message merged in between cannot hide the gap. This is
the twin of `ChannelsModel.rereadOnReconnect`, and is off by default, so the Mac is unchanged.
Alternatives considered:
- *A separate iOS timeline model*: two copies of paging, merging and marking read.
- *The server's `after=` forward sync*: core's history call takes only `before`; the spec
  rejected a core change for this (§4).

**The iOS lifecycle object: `ConversationSession`, like `SignedInSession`.** A view cannot be
driven from a unit test, which is how a missing `stop()` went unnoticed before (see
`SignedInSession.swift`). So the open conversation's start, stop and scene-phase handling live
in a small iOS class that the view only forwards to, and the tests drive that class.

**Closing on removal: when the row leaves the list, not when `closed` changes.** The iOS
conversation closes itself when its channel is no longer in `channels.channels`. That covers
both ways iOS learns of a removal: the live `channel.delete` event (which sets
`ChannelsModel.closed`), and a list re-read after a reconnect (which drops the row and never
sets `closed`). It also avoids the `closed` flaw (spec §5 risks: never cleared, so a second
removal of the same channel goes unnoticed). The flaw stays in the shared model for the Mac; see
section 5, decision 2.

**The follow rule: one shared function.** `ScrollToLatest.follows(away:mine:)` is `!away || mine`.
iOS calls it with `ScrollToLatest.isAway` from the scroll geometry, as the Mac computes
`awayFromLatest` today. The Mac keeps its current rule (it always follows the newest message);
changing the Mac is not in this issue.

## 2. Steps

Every step:
- is one pull request. A step built by more than one implementer is several commits in that one
  pull request, pushed and merged together (step 1);
- has each commit made with `git commit -s`, by `brook-committer`, from the diff and the reason;
- starts each new file with the two SPDX lines in its comment syntax;
- leaves `clients/macos/build.sh test` and `clients/ios/build.sh test` green, and from step 1 on,
  the Rust checks green. On this Mac those are, from the repository root:
  `cargo fmt --all -- --check && cargo clippy --locked -p brook-core -p brook-ffi --all-targets -- -D warnings && cargo test --locked -p brook-core -p brook-ffi`.
  The GNOME client needs GTK and GStreamer, so it is built and tested in CI only. The KDE client
  is not built anywhere, not even in CI (it is not a default workspace member);
- keeps each new test only after it has been seen failing against the mutant the step names. The
  failing output is pasted into the pull request.

Commit subjects use `core:` for core and the binding's Rust, `apple:` for shared Swift and Apple
scripts, `mac:` for Mac-only changes, `ios:` for iOS-only changes and `docs:` for docs.

**`REQUIRED_TESTS`.** `clients/ios/build.sh test` fails unless every test named in its
`REQUIRED_TESTS` array shows as passed. Its check matches XCTest's output
(`Test Case '-[Brook.Class testName]' passed`), so only XCTest tests can be listed. Swift Testing
tests (`@Suite`, `@Test`, such as `ScrollToLatestTests`) still run, but cannot be listed. So every
new test in this plan is an XCTest. Each step names what it adds to the array.

**Counting the Mac's tests.** "The Mac's count" below means both numbers from the end of
`clients/macos/build.sh test`: XCTest's `Executed N tests` line and Swift Testing's
`Test run with N tests` line.

### Step 1: the direct send carries a `client_id`

Subagents, in this order, each making its own commit in the one pull request:
1. `brook-core-implementer`: core, the binding's Rust, and the three Rust callers in
   `clients/gnome` and `clients/kde` (see section 5, decision 4). Commit:
   `core: an optional client_id on the direct send`.
2. `brook-apple-implementer`: the Swift that the new binding signature breaks, and the composer.
   Commit: `apple: the composer's direct send reuses its draft id`.

The first commit alone changes the generated Swift signature, so it leaves the Mac app unable to
compile (`FfiBrookClient` no longer matches the Mac's `ChatClient`) and the iOS **test target**
failing (the shared `FakeRealtime` no longer matches `FfiBrookClientProtocol`). The iOS app itself
still builds: it does not compile `ChatClient` until step 2. That is why both commits are pushed
and merged as one pull request; do not merge the first by itself.

The spec itself is amended in the plan's own pull request, by `brook-spec-writer`, for the
decisions in section 5 that change what it says (1, 3, 6 and 7). Nothing for it in this step.

Review: `brook-reviewer`, briefed to also check the server and protocol side:
- `services/api/app/routers/channels.py`, `send_message` (the `client_id` branch, around line
  798): a repeated id returns the stored message with 200, a deleted one comes back as its
  tombstone, the same id in another channel is 409. The route needs no change;
- `docs/PROTOCOL.md` §1, the `POST /channels/{id}/messages` row: it already documents the optional
  `client_id` and its rules. **No PROTOCOL.md line is needed**, because the wire contract does not
  move: core starts sending a field the route has accepted since the outbox shipped. The reviewer
  confirms that no line in PROTOCOL.md says the direct send has no `client_id`;
- the draft id is `UUID().uuidString.lowercased()`, a valid UUID, so the server's UUID check
  accepts it.

This pull request changes core, which GNOME and KDE build. So it is not a Mac-only pull request
and does not take the Mac-only review path.

Files, commit 1:
- `core/src/client.rs`, `send_message`: a fourth argument `client_id: Option<&str>`. Build the JSON
  body as today, then insert `"client_id"` only when it is `Some`. Without one, the key is absent,
  not `null`. Update the doc comment: "With `client_id`, sending the same message again is safe:
  the server returns the one it stored (PROTOCOL.md §1)."
- `core/src/client.rs` tests: the existing `sends_message_and_parses_author` passes `None`.
- `bindings/apple/src/client.rs`, `FfiBrookClient::send_message`: a fourth argument
  `client_id: Option<String>`, passed as `client_id.as_deref()`. Update the doc comment the same
  way.
- `clients/gnome/src/chat.rs:1605` and `clients/gnome/src/keyring.rs:713`, and
  `clients/kde/src/chat.rs:311`: add `None` as the last argument. No GNOME or KDE behavior
  changes.

Tests first (commit 1), in `core/src/client.rs`'s test module, with wiremock:
- `send_message_sends_the_client_id_it_is_given`: mount a 201 answer, call
  `send_message("c1", "hi", None, Some("0b7c…"))`, then read the request body from
  `server.received_requests()`. Its `client_id` is that string.
- `send_message_without_a_client_id_sends_none`: the same with `None`. The body has no
  `client_id` key at all (`body.get("client_id").is_none()`).

Mutants:
- always insert `"client_id": client_id` (so `null` when `None`): the second test must fail;
- ignore the argument and send no key: the first test must fail.

Files, commit 2:
- `clients/macos/Brook/Chat/ChatModels.swift`:
  - `ChatClient.sendMessage` gains `clientId: String?`, matching the generated signature;
  - `ComposerModel.send()`: for a new message (not an edit), take the id from `draftId(...)`
    **before** the queued send, as today. Then:
    - on `local.unavailable`, keep the draft (remove today's `draft = nil` at line 629), and send
      directly with that id;
    - pass `clientId: id` to `client.sendMessage`;
    - after the direct send succeeds, set `draft = nil`, so the next message, even with the same
      text, gets a new id;
    - an edit passes no id (`editMessage` has none);
  - the doc comment on `send()` and the comment at today's line 629: a network failure may still
    have delivered the message, and sending the same text and quote again reuses its
    `client_id`, so the server keeps one copy. Keep the user-facing message as it is ("It may not
    have been sent: check the conversation before sending again."): the spec keeps it.
- `clients/macos/BrookTests/ChatModelTests.swift`, `FakeChat`: `sendMessage` gains `clientId:`
  and records it in a new `let sentIds = Mutex<[String?]>([])`. The existing `sent` strings stay
  as they are, so no existing assertion changes.
- `clients/apple-shared/BrookTests/TestSupport.swift`, `FakeRealtime.sendMessage`: the new
  signature.
- `bindings/apple/swift/BrookCore/Tests/BrookCoreTests/OfflineAPITests.swift:104` and
  `ChatIntegrationTests.swift:46,48`: pass `clientId: nil`.
- `ChatIntegrationTests.testChatRoundTripAgainstTheTestServer` (the same test, so `itest.sh`'s
  suite count of 1 is unchanged): send one message twice with the same `clientId:` and assert
  both answers have the same `id`. This is the end-to-end proof that the binding passes the id
  and the server keeps one copy.

Tests first (commit 2), in `ComposerModelTests` (`ChatModelTests.swift`). `FakeChat` has local data
off by default, so the queued send answers `local.unavailable` and the composer sends directly.
That is exactly the iOS path:
- `testADirectSendPassesAClientId`: send "hello"; `sentIds` has one entry, a lowercase UUID.
- `testSendingTheSameTextAgainAfterAFailureReusesItsClientId`: `sendFailure = .Network`, send
  "hello"; the text comes back; clear the failure, send again; both `sentIds` entries are equal.
- `testChangedTextGetsANewClientId`: fail with "hello", then send "hello!"; the ids differ.
- `testTheNextMessageAfterASuccessGetsANewClientId`: send "hi" (works), send "hi" again; the ids
  differ. Without this, the server would return the first "hi" for the second, and the second
  message would be lost.

Mutants:
- put back `draft = nil` in the `local.unavailable` branch: the retry test must fail;
- pass `clientId: nil` to the direct send: the first test must fail;
- skip `draft = nil` after a direct success: the last test must fail.

Run:
- the Rust checks (above). `cargo test -p brook-core` runs the two new core tests, and
  `-p brook-ffi` builds the binding;
- GNOME's two edits are compiled and tested by CI. KDE's one edit is compiled nowhere (CI does
  not build KDE either): the pull request says so, and the reviewer reads the one-word change;
- `clients/macos/build.sh test`: the Mac's count is `main`'s plus 4;
- `clients/ios/build.sh test`;
- `cd bindings/apple/swift/BrookCore && swift test --filter OfflineAPITests` (after a build has
  made the xcframework);
- `bindings/apple/itest.sh`, **required**: it is the end-to-end proof that the binding passes the
  id and the server keeps one copy. The worktree has no `.itest.env` (it is gitignored), so copy
  it from the main checkout with `install -m 600 <main checkout>/bindings/apple/.itest.env
  bindings/apple/.itest.env`. If the server it names is the local stack, the agent provisions
  that stack itself first (section 3a). The file is never printed, committed or pasted.

`REQUIRED_TESTS`: no additions. The iOS app does not compile `ComposerModel` until step 2.

If it stops here: the Mac's direct send (taken when local data is off) can be retried without
making a duplicate. GNOME, KDE and the server are unchanged. iOS is unchanged.

Docs made wrong: the doc comments named above, fixed in this step. PROTOCOL.md: none.

### Step 2: the conversation models move to `clients/apple-shared`

Subagents: `brook-apple-implementer` for commits 1 to 3, `brook-docs-writer` for commit 4. Four
commits, so the move itself is a pure rename:
1. `mac: the conversation models no longer reach AppKit` (Mac-only edits, files still in
   `clients/macos`);
2. `apple: share the conversation models with iOS` (only `git mv`, every file byte-identical);
3. `ios: require the moved conversation tests` (`clients/ios/build.sh` and one iOS test);
4. `docs: the conversation models are shared` (the two READMEs, below).

Why this order: `clients/ios/project.yml` already compiles everything under
`clients/apple-shared/Brook`. A file that still imports AppKit cannot be moved there without
breaking the iOS build at that commit. So the AppKit is cut out first, while the files are still
Mac-only.

Commit 1 files (all under `clients/macos`):
- New `Brook/Chat/PasteImport.swift`, with `import AppKit`, `import Foundation` and
  `import UniformTypeIdentifiers`:
  - `enum PasteImport`, cut from `Chat/Staging.swift` unchanged;
  - `extension ComposerModel { static let macImporter: (NSItemProvider) async -> DropImport.Outcome }`,
    holding today's default closure from `ChatModels.swift:522-527`, unchanged. A comment says
    why it lives here: it names `PasteImport`, which needs AppKit, and shared code has none.
- `Brook/Chat/Staging.swift`: `PasteImport` removed, and `import AppKit` removed (nothing else in
  the file uses it: `NSFileCoordinator`, `NSItemProvider` and `UTType` are Foundation and
  UniformTypeIdentifiers).
- `Brook/Chat/ChatModels.swift`:
  - the `importer` default becomes `{ await DropImport.copy($0) }`. The comment says: the Mac
    sets `macImporter` (which also writes out pasted images); iOS has no drop or paste screen
    yet, so the default is never called there;
  - `ComposerModel.explainFiles`, the `local.unavailable` text: "Sending files needs this Mac's
    storage, which isn't available yet." becomes
    `"Sending files needs this \(ThisDevice.name)'s storage, which isn't available yet."`, as
    the skeleton did for `SessionStore`'s messages. The Mac's text stays byte-for-byte the same;
    shared code no longer names the Mac.
- `Brook/Chat/ChatViews.swift`:
  - `ChatView.init` builds its composer through a new
    `static func makeComposer(channelId:client:timeline:pending:) -> ComposerModel`. It holds
    today's lines from `init` (the `onMessage` merge, `composer.pending`, `pending?.timeline`)
    plus `composer.importer = ComposerModel.macImporter`. A static function, so a test can build
    the exact composer the Mac ships;
  - `enum ScrollToLatest` is cut into a new `Brook/Chat/ScrollToLatest.swift` (with
    `import Foundation`, which provides `CGFloat`);
  - `MessageRow.excerpt(_:)`, `MessageRow.time(_:)` and `AttachmentRow.icon(_:)` are cut into a
    new `Brook/Chat/MessageText.swift` as `enum MessageText` with `excerpt(_:)`, `time(_:)` and
    `fileIcon(_:)`, bodies unchanged. The three `Self.` call sites in `ChatViews.swift` become
    `MessageText.`. These are the pure helpers the iOS rows need (spec §5);
  - `ComposerView.replyBanner` stays: iOS has no reply yet (section 5, decision 6).
- `BrookTests/ChatModelTests.swift`: `FakeChat` is cut into a new `BrookTests/FakeChat.swift`, and
  `ReplyBannerTests` into a new `BrookTests/ReplyBannerTests.swift` (it tests a Mac view). What
  stays in `ChatModelTests.swift` (`TimelineModelTests`, `ComposerModelTests`, `SaveModelTests`)
  only tests code that moves.

Test first (commit 1), Mac-only, new `BrookTests/MacComposerTests.swift`:
- `testTheMacComposerWritesAPastedImageOut`: build a composer with `ChatView.makeComposer(...)`
  over `FakeChat` with `local = true`. Attach one `NSItemProvider` that carries only TIFF data
  (built as `PasteImageTests` builds one). The staged file's name starts with "Pasted image" and
  ends in ".png".

Mutant: remove the `importer =` line from `makeComposer`. The provider then goes through
`DropImport.copy`, whose staged name is not "Pasted image…png". The test must fail.

And in the Mac-only `BrookTests/ThisDeviceTests.swift`, `testTheMacWordingIsUnchanged` gains
`ComposerModel.explainFiles(LoginError.Api(code: "local.unavailable", message: ""))`, pinned to
the literal as it reads on `main` today. Mutant: interpolate `ThisDevice.system` instead of
`ThisDevice.name` ("this macOS's storage"): it must fail.

Commit 2, pure `git mv` (no other change):
- to `clients/apple-shared/Brook/Chat/`: `ChatModels.swift`, `PendingModel.swift`,
  `TypingSearch.swift`, `Staging.swift`, `ScrollToLatest.swift`, `MessageText.swift`;
- to `clients/apple-shared/BrookTests/`: `FakeChat.swift`, `OfflineFakes.swift`,
  `ChatModelTests.swift`, `ReadWhenActiveTests.swift`, `ReactionTests.swift`,
  `TypingSearchTests.swift`, `PendingModelTests.swift`, `ScrollToLatestTests.swift`.

These test files need no AppKit and no Mac-only type (checked against `main`: they use
`FakeChat`, `FakeRealtime`, `msg`, `channel`, `Gate`, `isolatedDefaults` and the moved models).
If one turns out not to compile for iOS, it goes back to `clients/macos/BrookTests` in commit 2,
and the pull request says why. No test is rewritten to make it fit.

They stay on the Mac (they need Mac-only code): `OfflineTimelineTests` (`CacheFeed`),
`SendFilesTests`, `DropImportTests`, `PasteImageTests`, `SessionStoreOfflineTests`,
`ConversationTests`, `ReplyBannerTests`, `MacComposerTests`. They still compile, because the
shared tests folder is in the Mac test target too.

Staying on the Mac, as the spec says: `FileRowModel`, `CacheFeed`, `NSWorkspaceBridge`,
`ChatViews.swift`, `PasteImport.swift`.

What iOS now compiles but never calls: the file staging and drop import, `PendingModel`,
`SaveModel`, `SearchModel`. The spec accepts this (§5). Nothing in them needs AppKit.

Commit 3:
- `clients/ios/BrookTests/MessageWordingTests.swift`, `testNoMessageNamesTheMac`: its hand-made
  list gains the `explainFiles` `local.unavailable` text, now that iOS compiles it. Mutant: put
  "Mac" back as a literal in that text: it must fail;
- `clients/ios/build.sh`, `REQUIRED_TESTS` gains these 23, in this order. Six of the composer
  tests (from `testChangingTheQuoteGetsANewClientId` to
  `testARetryAnsweredWithADeletedMessageIsNotShown`) were added by step 1's pull request (#347)
  beyond the four it planned, and the moved file carries them:
  - `ComposerModelTests/testASendClearsAndHandsTheMessageOver`
  - `ComposerModelTests/testAFailedSendGivesTheTextBack`
  - `ComposerModelTests/testANetworkFailureDoesntClaimItWasntSent`
  - `ComposerModelTests/testADirectSendPassesAClientId`
  - `ComposerModelTests/testSendingTheSameTextAgainAfterAFailureReusesItsClientId`
  - `ComposerModelTests/testChangedTextGetsANewClientId`
  - `ComposerModelTests/testChangingTheQuoteGetsANewClientId`
  - `ComposerModelTests/testChangingThenRestoringTheTextGetsANewClientId`
  - `ComposerModelTests/testClearingTheBoxGetsANewClientId`
  - `ComposerModelTests/testStartingAnEditDropsTheDraft`
  - `ComposerModelTests/testDeletingAMessageDropsTheDraft`
  - `ComposerModelTests/testARetryAnsweredWithADeletedMessageIsNotShown`
  - `ComposerModelTests/testTheNextMessageAfterASuccessGetsANewClientId`
  - `TimelineModelTests/testHistoryAndLiveEventsMergeByIdInOrder`
  - `TimelineModelTests/testEditsReplaceAndDeletesStay`
  - `TimelineModelTests/testAnOlderPageAskedWhileTheHeadLoadsWaitsAndEndsAtTheStart`
  - `TimelineModelTests/testAFailedOlderPageIsRetryable`
  - `TimelineModelTests/testAnEmptyOlderPageIsTheStart`
  - `ReadWhenActiveTests/testInTheBackgroundAMessageIsOwedAndReadOnceActive`
  - `ReadWhenActiveTests/testInFrontItsReadAtOnce`
  - `TimelineEventsTests/testReadyRetriesTheOpenTimelinesFailedHead`
  - `TimelineReactionTests/testAnEventForAMessageHereAdjustsItsChips`
  - `TimelineReactionTests/testAResyncForgetsTheOrderingSoALowerSeqIsHeardAgain`

Run:
- `clients/macos/build.sh test`: the Mac's count is step 1's plus 1 (`MacComposerTests`;
  `ThisDeviceTests` only gains an assertion). Grep the
  output for `ComposerModelTests` and `TimelineModelTests`: they still run on the Mac, from their
  new path;
- `clients/ios/build.sh test`: the new required tests pass on iOS;
- `git show -M --stat` of commit 2: every file is a 100% rename;
- `grep -rn "import AppKit\|NSPasteboard\|NSImage\|NSApp\b" clients/apple-shared` prints nothing.

No Mac on-screen check: any Mac build of this app, installed or from this branch, shares the
owner's bundle id, defaults and Keychain (section 3a). `MacComposerTests` builds the very
composer `ChatView` uses, so it covers the one wiring commit 1 changes. The pull request says
that pasting into the running Mac app was not tried, and why.

If it stops here: both apps behave exactly as in step 1. The conversation models are shared, and
their tests run in both apps.

Docs, commit 4 (`brook-docs-writer`), made incomplete by this step and fixed in it:
- `clients/macos/README.md`, "Shape": the conversation models (`TimelineModel`, `ComposerModel`,
  staging, pending, typing and search) and their tests are shared now; `PasteImport`, the views,
  `FileRowModel` and `CacheFeed` stay on the Mac;
- `clients/ios/README.md`, "What is shared": the same list, from the iOS side, and a line that iOS
  compiles the file and pending code without calling it yet.

The spec needs no "as built" note for this step: it is the move §5 describes, apart from
decision 6, which the plan's pull request already wrote into the spec.

### Step 3: the shared timeline learns the iOS re-read

Subagent: `brook-apple-implementer`. Commit: `apple: the open conversation can re-read after a reconnect`.

All changes are in shared files. Nothing on the Mac passes the new flag, so the Mac path is
unchanged; the first test below proves the default.

Files:
- `clients/apple-shared/Brook/Chat/ChatModels.swift`, `TimelineModel`:
  - init gains `rereadOnReady: Bool = false`, last. Its doc comment, modeled on
    `ChannelsModel.rereadOnReconnect`: iOS only. iOS suspends the socket in the background and
    keeps no cache, so events sent meanwhile are lost; a re-read of the newest page is the only
    way to show them. The Mac's `CacheFeed` fills that hole, so it leaves this off;
  - `static let rereadPageBacks = 3`, with a comment: up to 150 older messages are fetched to
    find where the re-read meets what is shown; past that, replacing is cheaper than paging;
  - `private(set) var replaced = 0`: counts how often a re-read replaced the shown history. The
    view scrolls to the newest message when it changes, and an older page that started before
    it is dropped;
  - `private var gapAnchor: String?`: the newest message that was shown when a re-read was
    asked for, and not yet filled up to. The gap check compares the re-read's pages against it,
    not against whatever is newest when the fetch finally runs: a live message merged in between
    (a `message.new` that arrives right after `ready`, or the user's own sent message) is newer
    than the gap and would hide it;
  - new `private func noteGapAnchor()`: `if gapAnchor == nil { gapAnchor = messages.last?.id }`.
    Every trigger calls it synchronously, before anything else can be merged. Triggers that
    arrive before the next drain turn starts share one anchor, the first (oldest) one; a trigger
    that arrives while a turn runs sets a fresh anchor for the turn queued behind it (that turn
    started by clearing it, below);
  - new `@discardableResult func reread() -> Task<Void, Never>`. It calls `noteGapAnchor()`, then
    returns a task that runs `await fetchHead()` and, if there is no error and the newest id
    changed, `await markNewestRead()` (marks read when active, else sets `readOwed`). It does
    not touch the reaction marks: a foreground re-read with the socket still up missed no event.
    It returns the task, so a caller (and a test) can wait for it after the anchor was taken;
  - `.ready` in `apply(_:)`: with the flag, clear `reactionSeqs` and `reactionMineSeqs`, then call
    `reread()` (the anchor is taken right there, synchronously). Without it, `retryFailedHead()`
    exactly as today. The comment cites PROTOCOL.md §2 (`reaction.update`): clear the marks on
    every reconnect, then refetch, because a restored server may number from lower values again;
  - `.resync` in `apply(_:)`: with the flag, call `noteGapAnchor()` synchronously before today's
    `Task { await fetchHead() }`. The comment says why: the binding delivers the events it kept
    right after the `Resync` (`bindings/apple/src/client.rs`, around lines 174-180), so they are
    merged before the head fetch runs, and the newest shown message by then is newer than the
    events that were dropped. Without the flag, `.resync` is exactly as today;
  - `startHeadDrain()`: with the flag, each turn of the loop calls a new
    `private func fetchHeadFillingGap() async` instead of `fetch(before: nil)`. Without the flag,
    it calls `fetch(before: nil)` as today;
  - `fetchHeadFillingGap()`, with `loading` set for its whole run:
    1. take the anchor and clear it: `anchor = gapAnchor ?? messages.last?.id`, then
       `gapAnchor = nil`. No anchor was noted only for a first load or a failed head's retry, and
       then the newest shown message is the right one. Clearing it at the start lets a trigger
       during this turn note a fresh anchor for the turn queued behind it;
    2. fetch the newest page;
    3. it **meets** the shown messages when there is no anchor, the page is empty, or its oldest
       id is at or below `anchor` (ids are UUIDv7, so id order is time order);
    4. while it does not meet, and fewer than `rereadPageBacks` page-backs were asked: fetch
       `before:` the oldest fetched id. An empty page means the start of the channel was
       reached: everything since it is fetched, so that counts as meeting. Otherwise prepend the
       page and check again;
    5. met: `merge(fetched)`. Not met: replace. Keep only the shown messages **newer than the
       newest fetched one** (live messages and the user's own sends merged while the page-backs
       ran, which are not in `fetched`), then `atStart = false`, `olderFailed = false`,
       `replaced += 1`, and `merge(fetched)`. The `deleted` set is kept, so a message deleted
       earlier stays a tombstone;
    6. on success (met or replaced), as `fetch(before: nil)` does today: `headFailed = false`,
       `olderFailed = false`, `error = nil` (the anchor was already cleared at the start);
    7. any request failing, the head or a page-back: nothing fetched is merged, and the anchor
       is put back as the older of this turn's anchor and any fresh one a trigger noted during
       the turn (so the next try still compares against the oldest point that may have a gap),
       `error = "Couldn't load messages."`, `headFailed = true`. The shown messages stay, and the
       next `ready` or foreground tries again. Merging part of it would leave a hole in the
       middle of the conversation (section 5, decision 5);
  - `loadOlder()`: notes `replaced` when it starts, and the oldest message it will ask before.
    It checks `replaced` again every time just before it sends the older request, whether or not
    it waited on a running head fetch on the way (the cache read and the drain wait are both
    places a replace can land), and once more when the older page's answer lands (so
    `fetch(before:)` gains the noted value as an argument, used only for older pages). If a
    replace happened at either point, the oldest message it noted
    belongs to history that was replaced: the answer, if any, is dropped (no merge, no `atStart`,
    no `olderFailed`), and `loadOlder` starts over from the new oldest message. Starting over,
    rather than giving up, matters: the loader's spinner asks once, when it appears, and a
    dropped ask would leave it spinning. A replace needs a whole re-read, so this cannot spin.
    The comment says all of this.
- `clients/apple-shared/Brook/Chat/ScrollToLatest.swift`: `static func follows(away: Bool, mine: Bool) -> Bool`
  returning `!away || mine`, with a comment: at the bottom the view follows a new message;
  scrolled up, only the user's own message moves it (spec §7, decision 3). The Mac does not call
  it yet.
- `clients/apple-shared/BrookTests/FakeChat.swift`:
  - `let historyAsked = Mutex<[String?]>([])`: the `before` of each `channelHistory` call, in order;
  - `var olderGate: Gate?`: holds the next call that has a `before`, once, after recording it (as
    `cacheGate` does for the cache).

Tests first, a new shared `clients/apple-shared/BrookTests/TimelineRereadTests.swift`
(`@MainActor final class TimelineRereadTests: XCTestCase`). Message ids are zero-padded
(`"a05"`), so string order is time order:
1. `testWithoutTheFlagAReadyOnlyRetriesAFailedHead`: default init, a successful load, then
   `.ready`: still one history call. (This is the Mac's path: the flag is off by default.)
2. `testWithTheFlagEveryReadyRereadsTheNewestPage`: the flag on, a load, then two `.ready`
   events: three history calls.
3. `testARereadThatOverlapsMerges`: shown `a01…a03`; the re-read's page is `a03, a04`. Result
   `a01…a04`, and `historyAsked == [nil, nil]` (the load and the re-read, no page-back).
4. `testARereadThatDoesNotMeetPagesBackUntilAPageMeets`: shown `a05, a06`. Pages: `a10, a11`,
   then `a08, a09`, then `a06, a07`. Result `a05…a11`; `historyAsked` ends `[nil, "a10", "a08"]`.
5. `testARereadThatNeverMeetsReplacesAfterThreePagesAndResets`: shown `a01, a02`, with `atStart`
   true (from an empty older page) and `olderFailed` set before (from a failed one). Pages `a20`,
   `a18`, `a16`, `a14`. Result `a14, a16, a18, a20`; `atStart` and `olderFailed` false;
   `replaced == 1`; exactly 4 history calls for the re-read.
6. `testAnOlderPageStartedBeforeAReplacingRereadIsDiscarded`: shown `a01, a02`; `loadOlder()`'s
   request (`before: a01`) is held by `olderGate`; a `.ready` re-read replaces with `a14…a20`;
   open the gate with `a00`. `a00` is not shown, `atStart` is false, and `loadOlder` asked again
   with `before: a14`.
7. `testAReadyDuringThePageBackJoinsIt`: hold the first page-back with `olderGate`, and deliver
   a second `.ready` while it is held: no new head request starts while it is held
   (`historyAsked` is `[…, nil, "aNN"]`). Release it: exactly one more head fetch follows
   (the request that arrived during the drain, as for a resync today), and no two fetches ever
   overlap.
8. `testAPageBackThatFailsKeepsTheShownMessagesAndSaysSo`: a page-back answers
   `LoginError.Network`: the shown messages are unchanged, `error` is set, nothing new merged.
9. `testARereadWhileActiveMarksTheNewestRead`: `isActive: { true }`; a re-read brings `m2`:
   `read.last == "m2"` and `readOwed` is false.
10. `testARereadWhileInactiveLeavesTheReadOwed`: `isActive` false: nothing read, `readOwed` true;
    then active and `appBecameActive()`: `m2` read.
11. `testAReadyClearsTheReactionMarks`: a `reactionUpdate` with `seq: 10` applies; a `.ready` (the
    re-read's page returns the message unchanged); then one with `seq: 5` applies too.
12. `testAForegroundRereadKeepsTheReactionMarks`: the same, but `await t.reread().value` instead
    of `.ready`: the `seq: 5` event is dropped.
13. `testAReplaceKeepsMessagesThatArrivedDuringThePageBack`: shown `a01, a02`; `.ready`; the head
    page is `a20`, and the first page-back is held by `olderGate`. While it is held, deliver
    `messageNew(a25)`. Release it; the pages are `a18`, `a16`, `a14`, so the re-read replaces.
    Result: `a14, a16, a18, a20, a25`.
14. `testTheGapAnchorIsTakenWhenTheReadyArrives`: shown `a05, a06`. Apply `.ready` and then, at
    once, `messageNew(a12)`, before the drain runs. The head page is `a10, a11, a12`: a page-back
    happens (`historyAsked` has `"a10"`), and with pages `a08, a09` and `a06, a07` the result is
    `a05…a12`.
15. `testAnOlderPageWaitingOnAReplacingRereadAsksFromTheNewOldest`: shown `a01, a02`;
    `loadOlder()` is held inside its cache read (`local = true`, a cache page that needs the
    network, `loadFails`, and `cacheGate`), after it noted `a01` as its oldest. A `.ready`
    re-read replaces with `a14…a20` meanwhile (whether `loadOlder` then waits on the drain or
    finds it finished does not matter: the check runs just before the request). When it is
    released, `loadOlder` asks `before: a14`, never `before: a01`.
16. `testAResyncTakesTheGapAnchorWhenItArrives`: test 14 with `.resync` in place of `.ready`:
    shown `a05, a06`; apply `.resync` and at once `messageNew(a12)`; head `a10, a11, a12`, then
    `a08, a09` and `a06, a07`: a page-back happens and the result is `a05…a12`.

And a new shared `clients/apple-shared/BrookTests/ScrollFollowTests.swift` (XCTest):
17. `testFollowsANewMessageAtTheBottom`: `follows(away: false, mine: false)` is true.
18. `testStaysPutForSomeoneElsesMessageWhenAway`: `follows(away: true, mine: false)` is false.
19. `testFollowsTheUsersOwnMessageEvenWhenAway`: `follows(away: true, mine: true)` is true.

Mutants, each must fail the named test:
- default `rereadOnReady = true`: 1;
- the re-read fetches outside `headDrain` (directly with `fetch`): 7;
- `rereadPageBacks = 4`: 5;
- the replace leaves `atStart` as it was: 5;
- no `replaced` check when the older answer lands: 6;
- no `replaced` check just before the older request is sent: 15;
- the replace sets `messages = []` before merging: 13;
- the anchor read when the drain runs (`messages.last?.id` there) instead of in `reread()`: 14;
- no `noteGapAnchor()` in `.resync`: 16;
- merge what was fetched when a page-back fails: 8;
- `reread()` never marks read: 9;
- clear the marks in `reread()` instead of in `.ready`: 12; do not clear them at all: 11;
- `follows` ignores `mine`: 19.

`REQUIRED_TESTS` gains all 19, as `TimelineRereadTests/<name>` and `ScrollFollowTests/<name>`.

Run: `clients/macos/build.sh test` (the Mac's count is step 2's plus 19: the shared tests run on
the Mac too, and test 1 there is the proof that the Mac's `ready` only retries a failed head);
`clients/ios/build.sh test`.

If it stops here: both apps behave as in step 2. The re-read exists and is tested, but nothing
turns it on.

Docs: the comments are written with the code. In the same pull request, `brook-spec-writer`
adds the spec's "as built" note for this step: the anchor taken when the re-read is asked for,
the replace keeping messages newer than the fetched ones, and an older page that started
before a replace asking again from the new oldest.

### Step 4: the iOS conversation screen (read, scroll back, stay live)

Subagents: `brook-apple-implementer` (commit `ios: open a channel or DM and read it`), then
`brook-docs-writer` and `brook-spec-writer` for the docs commit (`docs: iOS opens and reads a
conversation`).

Files:
- New `clients/ios/Brook/ConversationSession.swift`, `@MainActor final class ConversationSession`.
  The view only forwards to it, so its lifecycle can be tested without a view (as
  `SignedInSession`):
  - `init(channelId:channels:client:me:isActive:)`, where `client` is `any ChatClient` and
    `isActive` defaults to `{ AppActivity.isActive }`. It builds the `TimelineModel` with
    `me`, `members:` from the channel's row, `isActive` and `rereadOnReady: true`. It also keeps
    the row's title from the moment it opened, for the moment after a removal;
  - `start() async`: `channels.openChannel = channelId` (this clears the row's badge),
    `channels.timeline = timeline` (the message events now reach it), then
    `await timeline.load()`. Order matters: events that arrive during the load are not lost;
  - `stop()`: clears `channels.openChannel` and `channels.timeline` **only when
    `channels.openChannel == channelId` and `channels.timeline` is either nil or this session's
    own timeline** (compared by identity). Both halves matter:
    - a fast back-then-open can start the next conversation, even of the same channel, before
      this one stops; its timeline is then the open one, and it must not be cut off;
    - after a removal, `ChannelsModel.cacheRemoved` has already set `timeline = nil` but left
      `openChannel` set (`ChannelsModel.swift`, `cacheRemoved`). An identity check alone would
      then leave `openChannel` set for good, and a re-added channel's mention badge would never
      show again;
  - `@discardableResult func sceneChanged(from:to:) -> Task<Void, Never>?`, synchronous: when the
    scene becomes `.active` from anything else (as `ForegroundReload` does for the list), it
    calls `timeline.reread()` at once, so the gap anchor is taken at the scene change (step 3),
    and returns a task that waits for that re-read and then calls `timeline.appBecameActive()`.
    After, not before: the re-read may bring newer messages, and what was owed is then marked
    against the newest one. Otherwise it returns nil;
  - `var title: String`: `channels.title(row)` while the row exists, else the title kept at open;
  - `var isRemoved: Bool`: the row is no longer in `channels.channels`. A comment says why this
    and not `channels.closed` (section 1).
- `clients/ios/Brook/SignedInSession.swift`: keeps `let me: String`, which the conversation needs.
- `clients/ios/Brook/BrookApp.swift`: `SignedInHome` passes its `client` on to
  `ChannelListView`, as the conversation's `ChatClient`.
- `clients/ios/Brook/ChannelListView.swift`:
  - `NavigationStack` with a `.navigationDestination(for: String.self)` that shows
    `ConversationHost` for that channel id;
  - each row becomes a `NavigationLink(value: row.id)` around today's `ChannelRowView`;
  - the comments made wrong are corrected here, in the step that makes them wrong: "rows do not
    open anything yet" goes; the unread-count comment says the real reason: the server sends
    `unread_count` in `GET /channels`, but the Apple binding's `FfiChannel` drops it
    (`bindings/apple/src/types.rs`), so only mentions can be shown (its own issue, spec §3).
- New `clients/ios/Brook/ConversationView.swift`:
  - `ConversationHost`: `@State var session: ConversationSession?`, built once in `.task` and
    guarded with `if session == nil`. While it is nil, the body shows a `ProgressView`, never an
    empty `Group`. The comment cites #335: on a real iPhone a `.task` on an empty `Group` never ran;
  - `ConversationView`: `.navigationTitle(session.title)` with
    `.navigationBarTitleDisplayMode(.inline)`. `.task { await session.start() }`,
    `.onDisappear { session.stop() }`, and
    `.onChange(of: scenePhase) { old, new in session.sceneChanged(from: old, to: new) }`: a
    plain call, not inside a `Task`, so nothing can be merged between the scene change and the
    anchor.
    `.onChange(of: session.isRemoved, initial: true)` calls `dismiss()` when it becomes true;
  - the message list, plain SwiftUI, mirroring the Mac's structure:
    - a `ScrollView` with a `LazyVStack` (with `.scrollTargetLayout()`);
    - at the top, the same three states as the Mac: "This is the start of the conversation."; a
      `ProgressView` whose `.onAppear` calls `loadOlder()`; or "Couldn't load older messages.
      Retry" as a button;
    - the rows, then a clear spacer with id `bottom`, of height `ScrollToLatest.gap`;
    - `timeline.visibleError` under the list ("Couldn't load messages.");
    - `.defaultScrollAnchor(.bottom, for: .initialOffset)`: it opens at the newest message;
    - keeping the reading position when an older page lands at the top: track the position with
      `.scrollPosition(id:)` over the row ids, which is meant to keep the identified row in place
      when rows are inserted above it. If the simulator check shows a jump anyway, fall back to: note the
      first row's id before `loadOlder()`, and after it returns, `proxy.scrollTo(thatId,
      anchor: .top)` without animation. A comment beside it records which one held, and how it
      was checked;
    - `.onScrollGeometryChange` computes `away` with `ScrollToLatest.isAway`, as the Mac does;
    - `.onChange(of: timeline.messages.last?.id)`: scroll to `bottom` when the old value was nil
      (the first page), or when `ScrollToLatest.follows(away:mine:)` says so, with `mine` meaning
      the newest message's `authorId == me`;
    - `.onChange(of: timeline.replaced)`: scroll to `bottom` (the history was replaced, spec §4);
    - while `away`: a jump-to-latest button at the bottom trailing corner, a `chevron.down` in a
      circle, `.accessibilityLabel("Jump to the latest message")`. Tapping it scrolls to `bottom`.
  - `MessageRowView` (iOS's own, spec §5), from the timeline's data and the shared helpers:
    - the author is `timeline.authorName(message, showUsernames: channels.showUsernames)`. iOS
      has no Show usernames setting, so it shows display names;
    - the time is `MessageText.time`; "edited" when `editedAt` is set and the message is not
      deleted;
    - the reply's line is "↳ Replying to \(MessageText.excerpt(quote))";
    - the body has `.textSelection(.enabled)`, which gives the system's long-press Copy;
    - "Message deleted" for a tombstone; "Files removed" for an empty message with no files, as
      on the Mac;
    - each file shows its `MessageText.fileIcon`, its name and its size
      (`ByteCountFormatter`, `.file`), with no actions;
    - each reaction shows as plain text, "emoji count", with an accessibility label like the
      Mac's. Reactions cannot be tapped (spec §3).
- New `clients/ios/BrookTests/ConversationSessionTests.swift` (below).
- `clients/ios/build.sh test`: also run `"$ROOT/clients/macos/check-person-names.sh" "$HERE/Brook"`
  before the build, as the Mac runs it on its own folder. The new rows show people's names; this
  keeps them going through `PersonName`.

Tests first, `ConversationSessionTests` (iOS-only; `FakeRealtime` for the list, `FakeChat` for
the conversation, isolated defaults, `drainMain()` after each delivered event):
- `testOpeningSetsTheOpenChannelAndItsEventsAndLeavingClearsThem`: after `start()`,
  `channels.openChannel == "c1"` and a delivered `messageNew` for `c1` is in the timeline. After
  `stop()`, both are cleared and the next `messageNew` is not.
- `testLeavingAnOlderConversationLeavesTheNewerOneOpen`: start A, start B, stop A:
  `openChannel` is still B, and B still gets events.
- `testLeavingAndReopeningTheSameChannelKeepsTheNewOneOpen`: start S1 for `c1`, start S2 for
  `c1`, stop S1: `openChannel` is still `c1`, `channels.timeline` is S2's timeline, and S2 still
  gets events.
- `testTheOpenConversationRereadsOnEveryReady`: after `start()`, two `.ready` events through
  `FakeRealtime` each add one history call. This proves iOS passes the flag.
- `testComingBackToTheForegroundRereadsTheConversation`, waiting with
  `await s.sceneChanged(from:to:)?.value`: `.inactive → .active` adds a history call, and then
  reads what was owed (`isActive` false while a `messageNew` arrives, then true: `read` gains the
  newest id); `.active → .inactive` and `.background → .inactive` add none and return nil.
- `testAChannelGoneFromTheListClosesTheConversation`: `isRemoved` turns true after a
  `channelDelete` event, and then `stop()` (what the view's dismiss leads to) leaves
  `channels.openChannel` nil. It also turns true after a list re-read whose answer no longer
  has the channel (the reconnect path, which never sets `closed`).
- `testOpeningClearsTheRowsMentionBadge`: a row with `unreadMentions: 1`; after `start()`,
  `channels.mentions(row)` is nil.

Mutants:
- `stop()` leaves `openChannel` set: the first test must fail;
- `stop()` clears unconditionally: the second must fail;
- `stop()` guards by `openChannel == channelId` alone: the third must fail;
- `stop()` guards by timeline identity alone: the sixth must fail on its `stop()` half;
- the session builds the timeline without `rereadOnReady: true`: the fourth must fail;
- re-read only on `.background → .active`: the fifth must fail (the real path back is
  `background → inactive → active`); `appBecameActive()` dropped from it: the fifth must fail too;
- `isRemoved` reads `channels.closed == channelId`: the sixth must fail on its re-read half.

`REQUIRED_TESTS` gains the seven `ConversationSessionTests/<name>`.

Run: `clients/ios/build.sh test`; `clients/macos/build.sh test` (unchanged count: nothing shared
changed). Then the simulator checks against the local stack (section 3a), with
`clients/ios/build.sh run`, and screenshots (`xcrun simctl io booted screenshot`) in the pull
request:
- Done 1: a channel opens with `#name` as title and a DM with the other person's name; the newest
  messages are at the bottom; Back returns to the list;
- Done 2: in the seeded channel (over 100 messages), scrolling up reaches "This is the start of
  the conversation." without the reading position jumping when a page lands (two screenshots:
  before and after a page lands, the same message at the same height). With `caddy` stopped,
  scrolling up shows "Couldn't load older messages. Retry". With it back, two outcomes pass:
  tapping Retry loads the page, or, within seconds and without a tap, the socket reconnects, its
  `ready` re-read succeeds and clears `olderFailed`, the spinner comes back and asks by itself.
  The pull request says which one happened;
- Done 4: account B sends, edits, deletes and reacts: each shows. Scrolled up, B's new message
  does not move the view and the jump-to-latest button shows; tapping it goes to the newest;
- Done 5: B mentions A; the row shows `@1`; opening clears it; back, pull: still clear; a new
  mention after going back shows it again;
- Done 6, with the dead-socket harness of section 3a: the three messages B posted while the app
  was in the background and `caddy` was stopped show without reopening and without a pull; the
  api log shows the re-read's `GET /channels/{id}/messages` and the new WebSocket connection
  after the return; back on the list, a pull shows no mention badge for that channel;
- Done 8, second half: B removes A from the open channel: the phone goes back to the list, and
  the channel is gone.

If it stops here: on iOS a person can open, read, scroll back and follow a conversation live. No
box yet, so it is read-only. The Mac is unchanged.

Docs, in this pull request:
- the `ChannelListView.swift` comments (above, in the code commit);
- `clients/ios/README.md` (`brook-docs-writer`): "What the app does today" gains the reading
  side: open a channel or DM, read, scroll back, live new, edited and deleted messages and
  reactions, jump to latest, read state, back to the list on removal. "Known limits" drops
  "Nothing opens a channel yet" and "Nothing opens a channel yet either", and the unread-count
  reason becomes the real one (the server sends `unread_count`; the Apple binding drops it). It
  adds: no cache, so each opening and each older page is fetched again, and with no network an
  open conversation shows only the error; and, until step 5, no message box. "iOS-only" gains
  `ConversationSession.swift` and `ConversationView.swift`;
- `docs/PROTOCOL.md` §2, `reaction.update`'s "Known client gap" (`brook-docs-writer`): it names
  the Mac and GTK. iOS now runs the Mac's model, so it has the same gap; add iOS;
- the spec's "as built" note for this step (`brook-spec-writer`): which scroll API held the
  reading position, and how it was checked.

### Step 5: the iOS message box

Subagents: `brook-apple-implementer` (commit `ios: send a text message from the conversation`),
then `brook-docs-writer` and `brook-spec-writer` for the docs commit (`docs: iOS sends text`).

Files:
- `clients/ios/Brook/ConversationSession.swift`:
  - builds `let composer = ComposerModel(channelId:client:onMessage:)`, whose `onMessage` merges
    the sent message into the timeline (`[weak timeline]`, as on the Mac). The live echo of it
    then merges into the same row;
  - `var archived: Bool`: the row's `archived` (it updates live through `channel.update`).
- `clients/ios/Brook/ConversationView.swift`, a new `ComposerBar`, placed with
  `.safeAreaInset(edge: .bottom)`, so it sits above the keyboard:
  - `TextField("Message", text: $composer.text, axis: .vertical).lineLimit(1...6)`. No
    `.onSubmit`, so Return adds a new line (spec §4);
  - a Send button, `.disabled(!composer.canSend)`, which runs `await composer.send()`;
  - `composer.error` above the field, in red;
  - while `session.archived`: no field, and instead "This channel is archived. An owner or admin
    can unarchive it." (the Mac's words);
  - `.onChange(of: session.archived, initial: true) { composer.readOnly = $1 }`, as the Mac does;
  - the message list gets `.scrollDismissesKeyboard(.interactively)`;
  - when the field gains focus and the view is not `away`, scroll to `bottom`, so the newest
    message stays visible above the keyboard;
  - a send scrolls to the newest message through step 4's follow rule (`mine` is true).

Tests first, in shared `ComposerModelTests` (`clients/apple-shared/BrookTests/ChatModelTests.swift`)
and in iOS `ConversationSessionTests`:
- `ComposerModelTests/testOnlySpacesCannotBeSent`: text `"  \n "`: `canSend` is false; `"hi"`:
  true. Mutant: drop the trim in `canSend`: it must fail.
- `ComposerModelTests/testASignedOutSendSaysSo`: `sendFailure = .NotAuthenticated`: the text
  comes back and the error is "You were signed out.". Mutant: map `.NotAuthenticated` to
  "Couldn't send.": it must fail.
- `ConversationSessionTests/testASentMessageShowsOnceWhenItsEchoArrives`: after `start()`, send
  "hi": the timeline has `m9` before any event. Then deliver `messageNew(m9)`: still one `m9`.
  Mutant: an empty `onMessage`: it must fail.
- `ConversationSessionTests/testArchivingTheChannelMakesItArchived`: deliver a `channelUpdate`
  with `archived = true`: `session.archived` turns true. Mutant: read `archived` once, at init:
  it must fail.

`REQUIRED_TESTS` gains those four.

Run: `clients/ios/build.sh test`; `clients/macos/build.sh test` (the Mac's count is step 4's plus
2, the two shared composer tests). Then **the full simulator pass of Done 1 to 8** against the
local stack (section 3a), with screenshots, including what this step adds:
- Done 3: send from the phone: it shows once on the phone, and B's history (the API) has it
  once. Send is disabled while the box is empty or only spaces. With the keyboard up, the box
  and the newest message are visible; scrolling the messages dismisses the keyboard. Return adds
  a new line;
- Done 7: with the server stopped, Send puts the text back with "It may not have been sent:
  check the conversation before sending again."; with it back, Send again: one copy on the phone
  and one in B's history;
- Done 8, first half: B archives the open channel: within a few seconds the box is replaced by
  the archived note.

What the agent cannot check here, and the owner's check (step 6) covers: the Mac app showing
the phone's messages live (Done 3, 7), and every lifecycle step on a real device (Done 9).

If it stops here: the iOS app meets Done 1 to 8 in the simulator, and Done 10, 11 and 12. It
lacks only the device check (Done 9).

Docs, in this pull request:
- `clients/ios/README.md` (`brook-docs-writer`): "What the app does today" gains sending text,
  the archived note and the keyboard; the "no message box" limit from step 4 goes; what it does
  not do yet (spec §3): attachments (send, open, save, previews), reply, edit, delete and react
  from the phone, showing who is typing, drafts kept after leaving, notifications, offline
  history;
- the spec's "as built" note for this step (`brook-spec-writer`), and this plan through
  `brook-plan-writer` wherever a step was built differently from what it says.

No change to `docs/user-guide.md` and `docs/admin-guide.md`: the iOS app is not distributed yet,
and the guides describe no iOS app. No ADR: no decision here outlives this feature.

### Step 6: the owner's check on a real iPhone

Not an agent step. The owner runs it, against chat.madalin.me (https), in a dedicated test
channel, with the Mac app signed in as a second account. The app is installed from Xcode after
`clients/ios/build.sh` (see `clients/ios/README.md`, "Run on an iPhone"), then **quit and launched
from the Home Screen**, so no debugger keeps it from being suspended. Nothing is posted to real
channels.

- Done 1 to 8, as written in the spec, with the Mac as the other client. In particular, the
  parts the simulator pass could not show: the phone's message appears on the Mac without any
  action there (Done 3, 7).
- Done 9: each lifecycle step on the device: open, back, lock and unlock, Home Screen and back,
  with a conversation open. The navigation push and pop, `.task`, `.onDisappear` and the scene
  phase each have to work there, not only in the simulator (#335).
- The reading position when an older page lands, and the box above the keyboard, on the device
  (spec §5 risks).
- The first https run of the conversation calls on iOS (history, send, read, reactions).

The results go into a comment on #337, and into the spec's "as built" notes through
`brook-spec-writer`.

## 3. Migrations and production

No database, server or route change. Core sends one more JSON field on a route that has accepted
it since the idempotent send shipped (the outbox already sends it); the server needs no update. No local data
changes on either app: iOS keeps none, and the Mac's cache and outbox are untouched.

To roll back, revert the pull requests in reverse order, newest first.

### 3a. The local checks, in the simulator

The local stack is as in the iOS README ("Test against a local server"): from `deploy/`,
`make init` (first time), `make up`, and `http://127.0.0.1:8080` in the app. The app is launched
with `clients/ios/build.sh run`, which uses `simctl` and attaches no debugger.

- **Accounts.** Three throwaway accounts on the local stack: the admin (the first
  `POST /api/v1/auth/register` on an empty stack), then A and B, registered by the admin with
  `admin_password`. A signs in on the phone. B plays "the Mac" (section 5, decision 1): it
  creates the test channel (so it owns it), adds A, sends, edits, deletes, reacts, mentions A,
  archives and removes A. Passwords are generated at run time (`openssl rand -base64 18`) into a
  mode-600 file in the session's scratchpad, never printed and never on a command line. JSON
  bodies and bearer tokens go through files, as `bindings/apple/provision-itest-account.sh` does.
- **B's calls**, all from PROTOCOL.md §1: `POST /channels/{id}/messages` (send; with `@<A's handle>`
  to mention), `PATCH /channels/{id}/messages/{mid}` (edit), `DELETE /channels/{id}/messages/{mid}`
  (delete), `POST /channels/{id}/messages/{mid}/reactions` (react),
  `PATCH /channels/{id}` with `{"archived": true}` (archive),
  `DELETE /channels/{id}/members/{A's id}` (remove), and `GET /channels/{id}/messages` (to count
  copies of what the phone sent).
- **Seeding more than 100 messages**: B sends 120 numbered messages ("seed 001" … "seed 120") to
  the test channel in a shell loop, before the phone opens it.
- **"The network off"**: the simulator shares the Mac's network, and loopback keeps working with
  Wi-Fi off. So stop the server's front instead: `docker compose stop caddy` (from `deploy/`),
  and `docker compose start caddy` to bring it back. Requests then fail at once with a network
  error, as with no network.
- **Done 6, with a socket that is certainly dead.** With a conversation open on the phone:
  1. send the app to the background (below) and note the time;
  2. `docker compose stop caddy`: the phone's WebSocket goes through Caddy, so it is now dead,
     whatever iOS does with a suspended socket;
  3. B posts the three messages (one mentioning A) **straight to the api container**, which
     Caddy does not front: `docker compose exec -T api python -c '…'`, a few lines of
     `urllib.request` posting to `http://localhost:8000/api/v1/channels/<id>/messages` (the api
     image has Python and no curl). B's token and the message bodies are read from stdin, never
     put on the command line;
  4. wait 30 seconds, `docker compose start caddy`, and bring the app back;
  5. paste `docker compose logs --since <the noted time> api`: it must show, after the return,
     the conversation's `GET /api/v1/channels/<id>/messages` (the re-read) and a new WebSocket
     connection (the reconnect, with its sign-in).
- **Background and foreground**: send the app to the background by launching another app
  (`xcrun simctl launch booted com.apple.Preferences`), or lock it with the Simulator's Device >
  Lock. Bring it back with `xcrun simctl launch booted me.madalin.brook`, and check that the
  printed pid is the one from the first launch: the same process came back, it was not relaunched.
  Wait 30 seconds in the background, as Done 6 says.
- **Never a Mac build.** Any build of the Mac app, installed or from a branch, shares the owner's
  bundle id, defaults and Keychain, and the owner uses it against chat.madalin.me. Signing it in
  to the local stack would replace the owner's saved server and session. So the agent runs no
  Mac app at all; account B stands in for it.
- chat.madalin.me is used only in step 6, by the owner.

## 4. Risks

- **The reading position when an older page lands, and the box above the keyboard.** These are
  where SwiftUI scroll views most often misbehave, and no unit test can see them. Step 4 names a
  first approach and a fallback, the simulator check compares two screenshots, and step 6
  repeats it on the device.
- **Lifecycle differs between the simulator and the device** (#335). Push, pop, `.task`,
  `.onDisappear` and the scene phase are covered by `ConversationSessionTests` only as far as the
  session goes; the view wiring is checked in step 6 on the iPhone. `ConversationHost` keeps the
  `ProgressView` placeholder for the same reason as `SignedInHome`.
- **`ChannelsModel.closed` is never cleared.** iOS does not use it (section 1), so iOS is not
  affected. The Mac keeps the flaw: removed, re-added and removed again in one session, its
  selection stays on the channel. Section 5, decision 2.
- **The Mac's direct send now keeps its draft id until it succeeds.** If the id were kept after a
  success, a deliberate second "ok" would be swallowed by the server as a repeat of the first.
  `testTheNextMessageAfterASuccessGetsANewClientId` pins that the id goes after a success.
- **A message arriving while the app is inactive**, with a conversation open (Control Center
  pulled down, a call banner): the socket is still up, so the list counts it, and the re-read on
  return marks it read on the server. Back on the list, the row can show a stale `@1` until the
  next pull. The Mac has the same rule, and its cache corrects the count. Check it in step 5's
  pass (pull down Control Center, have B mention A, put it back, go back to the list). If it
  shows, it goes back to the main agent as a follow-up: a small shared `ChannelsModel` change,
  not part of this plan.
- **`AppActivity.isActive` and the scene phase can disagree for a moment.** The foreground
  re-read runs when the scene phase becomes active, and marks read through
  `UIApplication.applicationState`. If the state is not yet `.active` at that moment, the read
  is left owed and marked by `appBecameActive()` a moment later, so nothing is lost; Done 6's
  pull-to-refresh check shows whether the read arrived.
- **Flapping networks cause more requests.** Each `ready` re-reads (up to 4 requests). The drain
  runs one fetch at a time with at most one queued, so a burst of `ready` events costs at most
  two re-reads.
- **GNOME is compiled only in CI, and KDE nowhere.** Step 1 edits GNOME and KDE call sites. The
  local Rust checks cover core and the binding only; CI builds and tests GNOME; nothing builds
  KDE, not even CI. The KDE change is the literal `, None`, read by the reviewer.
- **The shared tests now run twice** (once per app): the iOS run grows by the moved conversation
  tests. Accepted, as for the skeleton.
- **iOS compiles code it never calls** (file staging, drop import, pending, save, search). The
  spec accepts it. It costs build time and nothing at run time.

## 5. Decisions for the owner

All seven decisions were approved by the owner on 2026-10-09; 3, 5, 6 and 7 each as
recommended below. The spec is aligned with them in this plan's pull request, by
`brook-spec-writer`.

1. **Approved. The second account in the agent's checks is the API, not the Mac app.** The spec's Done
   items run "with the Mac app signed in to the same server as a second account". In the
   agent's simulator checks against the local stack, account B acts through the server API
   instead (section 3a). The Mac app on this machine shares the owner's bundle id, defaults and
   Keychain, and is used against chat.madalin.me, so it must not be pointed at the local stack.
   The Mac app as the other client is in the owner's check (step 6), including "shows on the Mac
   without any action there" (Done 3, 7).
2. **Approved. `ChannelsModel.closed` stays as it is.** iOS closes when the row leaves the list, which also
   covers a removal seen only by a re-read. The Mac's flaw (spec §5 risks) gets its own issue.
   The alternative is to clear `closed` when a new channel opens, in the shared model, with a
   test: one line, but a Mac behavior change outside this issue.
3. **Approved. With the flag on, every newest-page fetch checks for a gap**: the first load, a `ready`, a
   foreground re-read, and a `resync`. That removes the spec's first known limit ("a `resync`
   merges the head page without this gap check") at no extra cost; the spec is amended in this
   plan's pull request.
   Keeping `resync` out would need a second kind of head fetch, and would leave the gap that
   events dropped inside core can open. Recommended: accept (the step 3 tests, including test
   16, assume it).
4. **Approved. Step 1's GNOME and KDE edits are made by `brook-core-implementer`, in the core
   commit.** They
   are one `None` argument each, forced by the core signature change, and CLAUDE.md would
   otherwise route them to `brook-linux-implementer`. In the core commit, every Rust commit
   builds. Whether GNOME and KDE should pass their own draft ids on their direct sends (GNOME has
   one at hand, `chat.rs:1597`) is a follow-up issue for each, not part of this plan.
5. **Approved. A page-back that fails keeps what is shown and says "Couldn't load
   messages."**, and the next `ready` or foreground tries again. The alternative, replacing with
   what was fetched, would show the newest messages sooner but drop history on any blip.
   Recommended: keep what is shown; it is the same rule as a failed newest page today.
6. **Approved. `ComposerView.replyBanner` stays on the Mac.** Spec §5 lists it among
   the helpers to share, but iOS has no reply yet, so nothing on iOS would call it. It moves with
   the reply issue. Recommended: keep it on the Mac; moving it now adds a Mac edit for no iOS use.
7. **Approved. "Joins it" for a second `ready` during a page-back** means: no second
   fetch runs beside it, and, as today for a `resync`, exactly one more newest-page fetch
   follows when the drain ends, with the fresh anchor that `ready` noted, because it may know of
   newer messages. Test 7 of step 3 pins this. Recommended: accept; it is the drain's existing
   rule, so no new behavior is added.
