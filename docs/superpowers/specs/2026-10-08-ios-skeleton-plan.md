# Plan: iOS app skeleton that signs in and lists channels (#272)

Spec: [2026-10-08-ios-skeleton-spec.md](2026-10-08-ios-skeleton-spec.md). Review: `brook-reviewer`
for every step; `brook-security-reviewer` also for steps 3 and 5 (the session's Keychain items and
data directory on a new platform, and the first-launch delete).

Changed after steps 1 to 7 merged, to match what was built: step 4 was two commits; the iOS
`prepare` gained a protected-data check first and reads its marker from the persistent domain
(section 1, step 5); step 6's path check is corrected (a path is accepted) and the pre-unlock
state is recorded; step 7 records `SignedInSession`, mention-only counts, the reconnect re-read
and the locked-launch Sign Out notice; step 8's owner check gains the pre-unlock, restore,
app-switcher and probe-error checks; the backup risk is settled (section 4).

## 1. Approach

**Sharing: one folder, `clients/apple-shared/`, listed by both XcodeGen projects.** It holds
`Brook/` (shared app sources) and `BrookTests/` (shared tests and their fakes). Each app compiles
those files into its own app module, named `Brook` in both projects, so the shared tests'
`@testable import Brook` compiles unchanged in both test targets and runs twice: once per app.
Files move there with `git mv`, so their history follows them.

Alternatives considered:
- *Moving the files into the `BrookCore` package.* Every type, init and property the apps use
  would have to become `public` (several hundred declarations), the Mac tests would change from
  `@testable import Brook` to the package module, and `BrookCore` would stop being "the
  bindings" and become app logic too. A larger diff for no gain.
- *A second local Swift package.* Has the same `public` cost, plus one more package to resolve.

**Seams: a same-named symbol that each app defines its own way.** Where shared code needs one
platform fact, it names a small type that each app provides in its own file. There are no
`#if os(...)` blocks in shared files, and no protocol where one symbol is enough:

| Symbol | Mac file (exists or new) | iOS file (new) |
|---|---|---|
| `AppActivity.isActive` | `clients/macos/Brook/AppActivity.swift` (unchanged, `NSApp`) | `clients/ios/Brook/AppActivity.swift` (`UIApplication.shared.applicationState == .active`) |
| `ThisDevice` (`name`, `system`, `fallbackServer`) | `clients/macos/Brook/ThisDevice.swift`: `"Mac"`, `"macOS"`, `"https://localhost"` | `clients/ios/Brook/ThisDevice.swift`: `"iPhone"`, `"your iPhone"`, `""` |
| `SessionPersistence.live()` | `clients/macos/Brook/SessionPersistence+Mac.swift` (today's code, moved out) | `clients/ios/Brook/SessionPersistence+iOS.swift` |

**Chains broken, each with the smallest change** (spec §5 table):
- `ChannelsModel` → `TimelineModel`: `var timeline: (any OpenTimeline)?`. `OpenTimeline` is a
  two-member `@MainActor` protocol (`apply(_ event: FfiServerEvent)`, `readOwed: Bool`), declared
  next to `ChannelsModel`. The Mac adds `extension TimelineModel: OpenTimeline {}`. Nothing else
  reads `channels.timeline` as the concrete type; every Mac and test site only assigns it.
- `ChannelsModel` → `AppActivity`: unchanged code; each app supplies `AppActivity` (above).
- `ChannelsModel` → `MacNotifier`: `Chat/Notifications.swift` is split. `NotificationPlanner` and
  `Notifying` are shared; `MacNotifier` moves to a new Mac file `Chat/MacNotifier.swift`. iOS
  passes no notifier (it is already optional).
- `SessionStore` → `CacheFeed`: a `@MainActor protocol LocalDataFeed: AnyObject` (`start()`,
  `stop()`, `checkLost()`) and a new **required** init parameter `makeFeed: FeedFactory?` (no
  default value), with
  `typealias FeedFactory = @MainActor (FfiBrookClient, UserDefaults) -> any LocalDataFeed`.
  Required, so every caller says `macFeed` or `nil` out loud: a Mac call site that forgot it is a
  compile error, not a silent loss of local data.
  `finishSignIn` starts local data only when persistence is `.on` **and** `makeFeed` is set. That
  one condition is the "local data off" switch the spec asks for: iOS passes `nil`.
  `feed` becomes `(any LocalDataFeed)?`. The Mac's one reader (`BrookApp.swift:50`) casts it with
  `store.feed as? CacheFeed`. The Mac defines `extension CacheFeed: LocalDataFeed {}` and
  `static let macFeed: SessionStore.FeedFactory = { CacheFeed(client: $0, defaults: $1) }`. Both
  `BrookApp` and the `SessionStoreOfflineTests` helper use `macFeed`, so the tests exercise the
  factory the app ships.
- `SessionStore.Message` Mac wording: the four messages that name the device interpolate
  `ThisDevice`, for example `"Couldn't reach the server. If \(ThisDevice.system) asked to allow
  local network access, allow it and try again."` and `"This \(ThisDevice.name) couldn't
  forget…"`. The two removal messages use `"this \(ThisDevice.name)'s data"`. The Mac text stays
  byte-for-byte the same, and a new test pins it (step 3).
- `SessionPersistence`: the shared file keeps only the enum (`.on`, `.off`, `.secondInstance`;
  `SessionStore` switches on all three, and iOS never produces `.secondInstance`). The rest moves
  to `SessionPersistence+Mac.swift`: `live()`, `choose(...)`, `groupSuffix`,
  `signedAccessGroup()` (which uses `SecTaskCreateFromSelf`, macOS-only) and `InstanceLock`.
  It is the Mac's policy (a signed group and one instance per user), and iOS has its own
  (step 5).
- `Settings.fallbackServer = ThisDevice.fallbackServer`. On iOS the server field is empty on a
  first launch, and the view shows the placeholder `https://chat.example.com`.

**xcframework slices depend on the caller.** `build-xcframework.sh` builds
`aarch64-apple-darwin` only, as today, unless it is passed `--ios`. With `--ios` it builds all
three slices (`aarch64-apple-darwin`, `aarch64-apple-ios`, `aarch64-apple-ios-sim`) and exports
`IPHONEOS_DEPLOYMENT_TARGET=26.0`. The macOS slice stays first: the bindings' metadata is read
from `SLICES[0]`'s dylib. `clients/macos/build.sh` keeps calling it with no argument, so the Mac
build pays nothing and needs no iOS Rust targets. `clients/ios/build.sh` passes `--ios`. The
macOS slice is in the iOS run too, because Done 6 checks for it.

**The race between the two build.sh scripts**: they share one output (the xcframework, the
generated Swift, `bindings/apple/build`), and the Xcode build reads it after it is written. Each
`build.sh` first re-executes itself under `/usr/bin/lockf -k bindings/apple/.build.lock` (this is
the system `lockf(1)`, already on every Mac). The lock covers the whole run: the xcframework
build and the `xcodebuild` that reads it. The kernel holds the lock, so a killed run releases it.
A second build waits; it does not fail. This is one line per script, plus a gitignore entry for
the lock file:

```bash
[[ -n "${BROOK_APPLE_LOCKED:-}" ]] || BROOK_APPLE_LOCKED=1 exec /usr/bin/lockf -k "$ROOT/bindings/apple/.build.lock" "$HERE/$(basename "$0")" "$@"
```

**liveCalls on `ready`**: in the shared `ChannelsModel.handle(_:)`, `.ready` sets
`liveCalls = [:]` before `ready = true`. Events reach the model in order on the main queue
(`EventBridge`), so the `channel.call` events the server re-sends after `ready` apply after the
clear. This lands in step 2, while the file is still in `clients/macos`, so the Mac gets the fix
even if everything after step 2 stops.

**First-launch Keychain cleanup (iOS only)**: `KeychainSlot` (BrookCore) gains
`public func deleteAll() throws`. It is one `SecItemDelete` with the slot query minus
`kSecAttrAccount`: class, service `dev.brook.Brook.datakey`, data-protection keychain, not
synchronizable, and the access group when one is set. That matches every `session:<origin>` slot
(and any other slot core ever wrote) under the service, whatever the server. On iOS
`SecItemDelete` removes every match. `errSecItemNotFound` counts as success.
`SessionPersistence.prepare(dataDir:calls:defaults:)` (iOS) runs these steps, in this order:
0. Check that files of the "until first user authentication" class can be read, with a probe
   file in `dataDir` written with that class (`UIApplication.shared` is still nil in
   `App.init`, so `isProtectedDataAvailable` cannot be used). Three outcomes: available (go on);
   locked, only for a permission refusal (EPERM, EACCES, or the Cocoa no-permission codes),
   which returns `.lockedUntilFirstUnlock` (step 6); any other failure, which returns `.off`.
   Before the first unlock the defaults read as empty and the delete finds nothing, so without
   this check the marker would be set while the old install's session is still there.
1. If the defaults key `KeychainCleared` is absent from the app's persistent domain (not
   `object(forKey:)`, which a `-KeychainCleared YES` launch argument would satisfy): call
   `deleteAll()`. On failure, return `.off`
   for this launch, leave the key unset (so the next launch tries again), and never restore. On
   success, set the key.
2. Create `dataDir`; on failure, `.off`.
3. Probe the `probe` slot as the Mac does; `FfiKeySlotError.Fatal` gives `.off`.
4. Return `.on(slot: KeychainSlot(accessGroup: nil), dataDir:)`.

`dataDir` is `Application Support/Brook` in the app container. The Keychain access group is
`nil`: the app's default group, its own (spec §4). It needs no entitlement. The marker is its own
key, not "defaults are empty", so a `defaults write … AllowInsecureHTTP` made in the simulator
before the first launch does not skip the cleanup. This runs in `BrookApp.init`, before
`SessionStore` exists, so it runs before anything can restore.

**Rust**: no Rust change is planned; the iOS slices of `brook-ffi` already build (spec §5). If
step 1 shows that the static library needs a crate-side fix to link or run on iOS (a feature
flag, a target-specific dependency), that one change goes to `brook-core-implementer` as step 1b
before step 1 is committed. Everything else is Swift, XcodeGen and the `bindings/apple` scripts,
done by `brook-apple-implementer`.

## 2. Steps

Every step:
- is one commit, made with `git commit -s` (the DCO check), by `brook-committer` from the diff;
- starts each new file (Swift, shell, YAML, xcconfig) with the two SPDX lines, in that file's
  comment syntax, after any shebang;
- leaves `clients/macos/build.sh test` green, and from step 1 on, `clients/ios/build.sh test`
  green too;
- keeps each new test only after it has been watched failing against the mutant the step names,
  with the output pasted into the step's evidence.

Commit subjects use `apple:` for shared and script changes, `mac:` for Mac-only changes and
`ios:` for iOS-only changes.

From step 1 on, `clients/ios/build.sh test` ends with a guard, modeled on `bindings/apple/itest.sh`:
a `REQUIRED_TESTS` array of `Class/testMethod` names that must each show as passed in the
`xcodebuild test` output (run through `tee`, exit code from `PIPESTATUS`). The guard catches a
shared test file that silently dropped out of the iOS target. Each step appends its tests to the
array.

### Step 1: the Rust core runs in an iOS app in the simulator (biggest risk first)

Subagent: `brook-apple-implementer`. Commit: `apple: iOS slices of the core and a skeleton iOS app that links it`.

Files:
- `bindings/apple/build-xcframework.sh`: an `--ios` argument appends `aarch64-apple-ios` and
  `aarch64-apple-ios-sim` to `SLICES` and exports `IPHONEOS_DEPLOYMENT_TARGET=26.0`. The
  `rustup target` check already loops over `SLICES`. Update the header comment: the iOS slices
  are built when the caller asks.
- `bindings/apple/swift/BrookCore/Package.swift`: `platforms: [.macOS(.v26), .iOS(.v26)]`, and
  update the comment ("iOS joins later" becomes: keep in step with both deployment targets in
  `build-xcframework.sh`). `BrookMedia`, `WebRTC` and `WebRTCAudioDevice` stay as they are; iOS
  depends only on the `BrookCore` product, so they are not built for iOS.
- `clients/macos/build.sh`: the `lockf` line, right after `HERE`/`ROOT` are set (before
  `test-install.sh`).
- `clients/macos/project.yml`: `BrookTests` gains the source
  `{path: ../apple-shared/BrookTests, name: AppleSharedTests}`.
- `git mv clients/macos/BrookTests/SmokeTests.swift clients/apple-shared/BrookTests/SmokeTests.swift`.
  It uses only `BrookCore` and `XCTest`, and becomes the core-link test of both apps.
- New `clients/ios/project.yml`:
  - project `Brook`; `bundleIdPrefix: me.madalin`; `deploymentTarget: {iOS: "26.0"}`;
  - package `BrookCore` at `../../bindings/apple/swift/BrookCore`;
  - base settings: `SWIFT_VERSION: "6.0"`, `SWIFT_TREAT_WARNINGS_AS_ERRORS: YES`,
    `IPHONEOS_DEPLOYMENT_TARGET: "26.0"`, `TARGETED_DEVICE_FAMILY: "1"` (iPhone only),
    `ARCHS: arm64` and `ONLY_ACTIVE_ARCH: YES`, as the Mac does. There is no x86_64 simulator
    slice, so an x86_64 simulator build must never be attempted;
  - target `Brook` (`application`, `platform: iOS`): sources `[Brook]`, product `BrookCore` only,
    `configFiles` Debug and Release `Signing.xcconfig`, `PRODUCT_BUNDLE_IDENTIFIER:
    me.madalin.brook`, `PRODUCT_NAME: Brook`, `MARKETING_VERSION: "0.0"`,
    `CURRENT_PROJECT_VERSION: "1"`. Generated `info` at `Brook/Info.plist`, with
    `CFBundleDisplayName: Brook`, `UILaunchScreen: {}`, `NSLocalNetworkUsageDescription` (the
    Mac's sentence) and `NSHumanReadableCopyright` (the Mac's). No entitlements file: the default
    Keychain access group needs none;
  - target `BrookTests` (`bundle.unit-test`, `platform: iOS`): sources
    `[{path: ../apple-shared/BrookTests/SmokeTests.swift, name: AppleSharedTests}]` only, a single
    file. Not the folder: from step 4 the folder holds shared tests whose subjects iOS does not
    compile until step 5. Not `clients/ios/BrookTests`: it does not exist yet, and XcodeGen
    2.46.0 stops on a missing source directory. Step 5 switches this to
    `[BrookTests, {path: ../apple-shared/BrookTests, name: AppleSharedTests}]`. Dependencies
    target `Brook` and package `BrookCore`, `TEST_HOST:
    $(BUILT_PRODUCTS_DIR)/Brook.app/Brook`, `BUNDLE_LOADER: $(TEST_HOST)`,
    `GENERATE_INFOPLIST_FILE: YES`, bundle id `me.madalin.brook.tests`;
  - scheme `Brook`, which tests `BrookTests`.
- New `clients/ios/Signing.xcconfig`: committed, comments only plus `#include? "Local.xcconfig"`.
  The comment says what goes in the gitignored `Local.xcconfig` for a device run:
  `DEVELOPMENT_TEAM = <team id>` and `CODE_SIGN_STYLE = Automatic`, the same team as the Mac app.
  Without that file, simulator builds sign to run locally, and device builds need `build.sh test`'s
  signing-off build or the file.
- New `clients/ios/build.sh`:
  - re-executes itself under the `lockf` line;
  - runs `build-xcframework.sh --ios`, then `xcodegen --quiet`;
  - modes:
    - no argument: build for the simulator;
    - `test`:
      1. check that the xcframework has directories `macos-arm64`, `ios-arm64` and
         `ios-arm64-simulator` (`ls` of `BrookCoreFFI.xcframework`); fail otherwise;
      2. `xcodebuild build -destination generic/platform=iOS CODE_SIGNING_ALLOWED=NO`;
      3. `xcodebuild test` on `-destination "platform=iOS Simulator,name=${BROOK_IOS_SIMULATOR:-iPhone 17}"`,
         with the Mac's `-test-timeouts-enabled YES -maximum-test-execution-time-allowance 60`;
      4. the `REQUIRED_TESTS` guard;
    - `run`: build for the simulator, `xcrun simctl boot` (ignore "already booted"),
      `open -a Simulator`, `xcrun simctl install`, then `xcrun simctl launch`. When
      `BROOK_ALLOW_INSECURE_HTTP=1` is set in the caller's environment, the launch passes it as
      `SIMCTL_CHILD_BROOK_ALLOW_INSECURE_HTTP=1`.
  - Derived data goes to `clients/ios/build.noindex`.
- New `clients/ios/Brook/BrookApp.swift`: a placeholder `@main` app whose one view shows
  `conversationLabel(kind: "public", name: "general", members: [], me: "", showUsernames: false)`.
  This is a call into Rust made by the **app** binary (not only the test bundle), visible on
  screen as `#general`. Steps 5 to 7 replace it.
- `.gitignore`: `clients/ios/Brook.xcodeproj/`, `clients/ios/build.noindex/`,
  `clients/ios/Brook/Info.plist`, `clients/ios/Local.xcconfig`, `bindings/apple/.build.lock`.

Tests first. `SmokeTests.swift` is moved, then gains two tests, so both apps run them. The
existing `testAppLinksAndCallsTheRustCore` covers only synchronous calls. The new ones reach
what a sign-in needs:
- `testAsyncLoginRunsOnTheRuntimeAndFailsOnTheNetwork`: a client for `https://brook.invalid`,
  then `try await client.login(handle: "x", password: "y")` must throw `LoginError.Network`.
  This runs core's tokio runtime, an async future handed across UniFFI, and DNS resolution.
  `.invalid` never resolves, so no socket connects and no TLS handshake happens.
- `testRestoreCallsBackIntoASwiftKeySlot`: a private recording fake `FfiKeySlot` (`load` returns
  nil and records the slot name), `enablePersistence(slot: fake, dataDir: <temp dir>)`, then
  `await client.restore()`. It must be `.notSignedIn`, and the fake must have recorded a load of
  a `session:` slot. This runs a Rust-to-Swift foreign callback.

All three are the first entries in `REQUIRED_TESTS`. Mutants to watch fail:
- the fake's `load` throws `FfiKeySlotError.Unavailable`: the restore test must fail
  (`.unavailable`);
- `clients/ios/build.sh test` with `--ios` removed from the `build-xcframework.sh` call: it must
  fail, at the slice check or at the link;
- `ios-arm64-simulator` dropped from `SLICES`: it must fail.

Run:
- `clients/ios/build.sh test`;
- `clients/macos/build.sh test`. The executed test count must be `main`'s plus the two new smoke tests; read it
  from the final "Executed N tests" line;
- `clients/ios/build.sh run`, with a screenshot of `#general` in the simulator;
- the lock: start `clients/ios/build.sh` in the background, then `clients/macos/build.sh`
  at once. The Mac run's `cargo build` must start only after the iOS run ends (paste the two
  timestamps).

If it stops here: the Mac app is unchanged except for a build that waits for a running iOS
build, and two more smoke tests. The iOS app shows one label. Nothing is shared yet except the
smoke tests.

Docs made wrong: the `build-xcframework.sh` header and the `Package.swift` comment (both fixed in
this step). `clients/ios/README.md` waits for step 8.

### Step 2: the Mac's call badges cleared on `ready`

Subagent: `brook-apple-implementer`. Commit: `mac: clear live call badges when the server says ready`.

Files:
- `clients/macos/Brook/Calls/ChannelsModel.swift`: in `handle(_:)`, `.ready` becomes
  `liveCalls = [:]` then `ready = true`. Add a comment saying why: the server re-sends one
  `channel.call` per running call right after `ready` and sends nothing for a call that ended
  while the socket was down (PROTOCOL.md, `ready`).
- `clients/macos/BrookTests/ChannelEventsTests.swift`: add `testReadyClearsLiveCalls`, with
  `FakeRealtime` and `started(...)`:
  1. deliver `.channelCall(channelId: "c1", callId: "k1", participantCount: 2)`; `drainMain`;
     `badge` is non-nil;
  2. deliver `.ready`; `drainMain`; `badge(c1)` is nil and `liveCalls` is empty;
  3. deliver `.channelCall` for `c1` again; `drainMain`; the badge is back.

Use the generated case's real labels; check them in `brook_ffi.swift`.

Test first: the test fails on today's code (the badge survives `ready`); paste that failure.
Mutant: move the clear after a re-sent `channel.call` would apply (clear in `.channelCall` when
the count is 0 only). The test must fail.

Run: `clients/macos/build.sh test`.

If it stops here: the Mac no longer shows a stale call badge after a reconnect. Nothing else
changes.

Docs made wrong: none (this is a bug fix; the spec already describes the behavior).

After the PR with this step merges, the main agent opens one issue each for GNOME and KDE
(CLAUDE.md §7). Neither clears call state on `Ready`: GNOME's `Ok(ServerEvent::Ready)` at
`clients/gnome/src/chat.rs:1024` clears only `reaction_order`, and KDE's at
`clients/kde/src/chat.rs:263` does nothing. Each issue states the behavior: after a reconnect, a
call that ended while the client was disconnected loses its badge.

### Step 3: shared code freed of AppKit, still in `clients/macos`

Subagent: `brook-apple-implementer`. Review: `brook-reviewer` and `brook-security-reviewer` (the
persistence split, the local-data switch). Commit: `mac: shared session and channel code no longer reaches AppKit`.

Files (Mac only; nothing moves yet):
- `Calls/ChannelsModel.swift`: the `OpenTimeline` protocol, and `var timeline: (any OpenTimeline)?`.
- `Chat/ChatModels.swift`: `extension TimelineModel: OpenTimeline {}`.
- `Chat/Notifications.swift`: keeps `NotificationPlanner` and `Notifying`, and drops
  `import UserNotifications`. `MacNotifier` is cut into a new `Chat/MacNotifier.swift`, with the
  `UserNotifications` import.
- `SessionStore.swift`: `LocalDataFeed`, `FeedFactory`, the required `makeFeed` init parameter
  and its stored property, `feed: (any LocalDataFeed)?`, and the start condition in
  `finishSignIn`. The four `Message` strings interpolate `ThisDevice`.
- Every `SessionStore(...)` call site adds `makeFeed:`, because it is required:
  - `macFeed` in `BrookApp.swift:37` and the `SessionStoreOfflineTests.swift` helper at ~27;
  - `nil` in `LoginFormTests.swift` (24, 55), `SessionStoreTests.swift` (28, 88, 248),
    `RestoreTests.swift:33` and `ScreenshotRenderer.swift:50`.

  `RestoreTests` passes `.on` persistence and, until now, also started local data on each
  sign-in. None of its assertions reads local data, so `nil` there changes nothing it checks.
- `Chat/CacheFeed.swift`: `extension CacheFeed: LocalDataFeed {}` and `SessionStore.macFeed`
  (in this file, so the shared `SessionStore` never names `CacheFeed`).
- `BrookApp.swift`: `SessionStore(persistence: .live(), makeFeed: SessionStore.macFeed)`;
  `feed: store.feed as? CacheFeed`.
- `SessionPersistence.swift`: the enum only. New `SessionPersistence+Mac.swift` gets `live()`,
  `choose`, `groupSuffix`, `signedAccessGroup()` and `InstanceLock`, with the `Darwin` and
  `Security` imports. The `.off` case's doc comment ("No keychain group in this build's
  signature (no provisioning profile yet, #79)…") is Mac-only. Reword it for both apps: nothing
  is stored, and quitting signs out. The Mac's reasons (no group, a refused group) move to
  `choose`; the iOS reasons (the cleanup failed, no data directory, a fatal probe) go to
  `prepare` in step 5.
- New `ThisDevice.swift`: `enum ThisDevice { static let name = "Mac"; static let system =
  "macOS"; static let fallbackServer = "https://localhost" }`, with a comment that the iOS app
  defines its own.
- `Settings.swift`: `fallbackServer = ThisDevice.fallbackServer`. The `defaults write` comment
  names both apps' bundle ids.
- `BrookTests/ChannelEventsTests.swift`: `testReadyRetriesTheOpenTimelinesFailedHead` (it needs
  `TimelineModel`) moves into `BrookTests/ReadWhenActiveTests.swift`, in its own
  `final class TimelineEventsTests`. That test calls `started(_:)` and `settle(_:)`, which are
  `private` to `ChannelEventsTests` (lines 14 and 22). Copy both into `TimelineEventsTests`, as
  private helpers. They are about ten test-only lines, and copying is cheaper than widening them
  into the shared support file.

Tests first:
- `SessionStoreTests.testNoFeedFactoryKeepsLocalDataOff`: persistence
  `.on(slot: UnusedSlot(), dataDir: "/data")`, `makeFeed: nil`, sign in with `FakeClient`. Then
  `localData == .off`, `feed == nil`, and `FakeClient.localCalls` has no enable. Mutant: drop
  `makeFeed != nil` from the condition, and start a feed-less enable. It must fail.
- `ThisDeviceTests` (Mac-only file `BrookTests/ThisDeviceTests.swift`): pins the four Mac strings
  as literals, exactly as they read on `main` today. Mutant: `ThisDevice.name = "Mac "`. It must
  fail.

Run: `clients/macos/build.sh test`. The count is the one from step 2, plus the two new tests.

If it stops here: the Mac app behaves exactly as before, and the code is ready to move.

Docs made wrong: the `.off` doc comment, which is fixed in this step.

### Step 4: move the shared files to `clients/apple-shared`

Subagent: `brook-apple-implementer`. Built as two commits, which keeps the move a pure rename:
first the `git mv` of the 19 files with the `project.yml` and `build.sh` edits, then the
test-support cut and paste into `TestSupport.swift`.

Every moved file is moved with `git mv` and left **byte-identical**, so git follows its history
(`git log --follow`, rename detection at 100%). The only edited files are:
- `clients/macos/BrookTests/CallModelTests.swift` and `ChatModelTests.swift`: helpers cut out
  (below);
- new `clients/apple-shared/BrookTests/TestSupport.swift`: those helpers pasted in;
- `clients/macos/project.yml` and `clients/macos/build.sh` (below).

The iOS project is not touched in this step.

Sources go to `clients/apple-shared/Brook/`:
- `ServerAddress.swift`, `Settings.swift`, `PersonName.swift`, `LoginForm.swift`;
- `SessionStore.swift`, `SessionPersistence.swift`;
- `Chat/OfflineClient.swift`, `Chat/Notifications.swift`, `Calls/ChannelsModel.swift`.

The test fakes move into a new `clients/apple-shared/BrookTests/TestSupport.swift`. This is a
cut, not a copy:
- from `CallModelTests.swift`: `FakeSubscription`, `FakeRealtime` and its big extension,
  `extension FakeRealtime: OfflineClient {}`, `Gate`, `FakeCallHandle`, `channel(_:_:)` and
  `drainMain()`;
- from `ChatModelTests.swift`: `msg(...)`.

Tests go to `clients/apple-shared/BrookTests/` with `git mv`:
- `FakeClient.swift`, `ServerAddressTests.swift`, `LoginFormTests.swift`,
  `SessionStoreTests.swift`;
- `ChannelEventsTests.swift`, `ChannelLabelTests.swift`, `ChannelOrderTests.swift` (it defines
  `isolatedDefaults()`), `ChannelsOfflineTests.swift` (it defines `cachedChannel`),
  `MentionCountTests.swift`, `PersonNameTests.swift`.

`SettingsTests`, `RestoreTests`, `NotificationsTests` and the rest stay in the Mac target: they
use `SettingsView`, `InstanceLock`, `choose`, `FakeNotifier`, `TimelineModel` or AppKit. If a
listed file turns out not to compile for iOS in step 5, it moves back to `clients/macos/BrookTests`
in step 5's commit, with the reason. No test is rewritten to force it.

Also:
- `clients/macos/project.yml`: the `Brook` target gains the source
  `{path: ../apple-shared/Brook, name: AppleShared}`;
- `clients/macos/build.sh test`: `check-person-names.sh` also runs on
  `$ROOT/clients/apple-shared/Brook`. It takes a directory argument already.

Test: no new test. The proof is that the Mac test count equals step 3's. Each moved test still
runs from its new path: grep the run's output for `ChannelEventsTests` and
`SessionStoreTests`. A missing shared folder in the Mac test target is a compile error, not a
silent loss: `CallModelTests` uses `FakeRealtime`.

Run:
- `clients/macos/build.sh test`;
- `clients/ios/build.sh test`. It still compiles only `SmokeTests.swift` from the shared tests
  folder (step 1's single-file source) and none of `apple-shared/Brook`, so the tests that just
  moved there do not reach the iOS target yet;
- `git show -M --stat HEAD` (step 4's own commit) shows each moved file as a 100% rename.

If it stops here: the Mac behaves as in step 3, with the files in their final place. The iOS
app is unchanged from step 1, and both builds pass. The moved shared tests run on the Mac only
until step 5.

Docs made wrong: `clients/macos/README.md` if it lists source files (check it; the docs step
fixes it).

### Step 5: the iOS app compiles the shared code; session persistence and the first-launch cleanup

Subagent: `brook-apple-implementer`. Review: `brook-reviewer` and `brook-security-reviewer`.
Commit: `ios: session kept in the Keychain, cleared after a reinstall`.

The shared sources, the three iOS seam files and the shared tests all enter the iOS project in
this one step, so the iOS test target never lists a test whose subject it does not compile.

Files:
- `clients/ios/project.yml`: the `Brook` target gains `{path: ../apple-shared/Brook, name: AppleShared}`.
  `BrookTests` switches from the single `SmokeTests.swift` to
  `[BrookTests, {path: ../apple-shared/BrookTests, name: AppleSharedTests}]`. From this step on,
  `clients/ios/BrookTests/` exists with real files.
- `bindings/apple/swift/BrookCore/Sources/BrookCore/KeychainSlot.swift`: `deleteAll()` (section 1).
- `bindings/apple/swift/BrookCore/Tests/BrookCoreTests/KeychainSlotTests.swift`: a
  `deleteAll` test with the file's recording fake. The query has the service, has no
  `kSecAttrAccount`, has the data-protection flag, and has the access group only when set;
  `errSecItemNotFound` does not throw; `errSecInteractionNotAllowed` throws `.Unavailable`.
- New `clients/ios/Brook/AppActivity.swift`, `ThisDevice.swift` and
  `SessionPersistence+iOS.swift` (`live()` and `prepare(dataDir:calls:defaults:)`, section 1).
- `clients/ios/Brook/BrookApp.swift`: builds `Settings()`, then
  `SessionStore(persistence: .live(), makeFeed: nil)`, and `LoginForm`. It keeps a placeholder
  signed-in view until step 7.
- `SessionPersistence+iOS.swift`, beside `live()`: a comment that the data directory stays in
  backups on purpose (section 4, backup).

Tests first (in `clients/ios/BrookTests/`):
- `SessionPersistenceIOSTests`, with a recording `SecItemCalls` fake, a temp directory and
  isolated defaults:
  1. a first launch deletes once with the service-wide query, sets the marker, creates the
     directory and returns `.on` with that path;
  2. a second launch does not delete;
  3. a failing delete returns `.off`, leaves the marker unset, and the next call deletes again;
  4. an uncreatable directory (a path under a regular file) returns `.off`;
  5. a `-34018` probe returns `.off`;
  6. `AllowInsecureHTTP` already set in the isolated defaults, marker absent (the simulator
     `defaults write` before a first launch): the delete still happens and the marker is set.

  Mutants:
  - set the marker before the delete: must fail case 3;
  - skip the cleanup when the defaults domain is not empty: must fail case 6;
  - skip the cleanup when `AllowInsecureHTTP` is set: must fail case 6.
- `MessageWordingTests`: every `SessionStore.Message` constant, listed by hand, plus
  `LoginForm.insecureWarning`'s text. None contains `"Mac"` or `"macOS"`. Mutant: the iOS
  `ThisDevice.name = "Mac"`. It must fail.
- From `apple-shared`, `SessionStoreTests.testNoFeedFactoryKeepsLocalDataOff` and
  `ChannelEventsTests.testReadyClearsLiveCalls` now run on iOS. Add both, the persistence tests
  and the wording test to `REQUIRED_TESTS`.

Run:
- `clients/ios/build.sh test`;
- `clients/macos/build.sh test`;
- `cd bindings/apple/swift/BrookCore && swift test --filter KeychainSlotTests` (it needs no
  server).

If it stops here: the iOS app compiles every shared file and its tests pass on a simulator. The
app still has no sign-in UI. The cleanup is in place before any session can be stored.

Docs made wrong, both fixed in this step:
- the `KeychainSlot` doc comments: add `deleteAll`'s purpose (an app reinstalled on iOS);
- `KeychainSlot.swift:36-37`, which says `accessGroup` is "nil only in tests and unsigned
  builds". The iOS app now passes nil on purpose, for its default access group.

### Step 6: iOS sign-in, code step and launch states

Subagent: `brook-apple-implementer`. Commit: `ios: sign in with password and two-factor code`.

Files:
- New `clients/ios/Brook/LoginView.swift`: a `Form` with Server, Handle and Password:
  - Server: `.textContentType(.URL)`, `.keyboardType(.URL)`, no autocapitalization or
    autocorrection, prompt `https://chat.example.com`;
  - Handle: `.textContentType(.username)`;
  - Password: a `SecureField` with `.textContentType(.password)`;
  - a Log In button; the error text; `form.insecureWarning`; `store.signOutWarning`.

  The code step shows:
  - a field with `.textContentType(.oneTimeCode)` and `.keyboardType(.numberPad)`;
  - a "Use a recovery code" toggle (`form.useRecovery`; when on, a plain field);
  - Back and Verify.

  The view adds no logic: everything it does goes through `LoginForm`, which the shared tests
  cover.
- `clients/ios/Brook/BrookApp.swift`: the root switches on `store.phase`, as the Mac does:
  `.restoring` shows `ProgressView("Signing in…")`; the signed-out phases show `LoginView`; and
  `.task { await store.restoreAtLaunch() }` restores at launch.
- Shared `SessionPersistence.swift`: a new case `.lockedUntilFirstUnlock` (the Mac never
  produces it, like `.secondInstance` on iOS). `SessionStore` starts it on the sign-in screen
  with `Message.waitingForFirstUnlock`: "Brook can't use its saved sign-in until your iPhone has
  been unlocked once after restarting. Unlock it, then close Brook and open it again." No
  automatic re-check: decided for simplicity.
- The recovery-code field is a `SecureField`, so the app-switcher snapshot cannot show it. The
  6-digit code stays a visible `.oneTimeCode` field, so iOS can fill it.

Tests: none for the view, which is wiring; `LoginFormTests` and `SessionStoreTests` (shared,
running on iOS) cover its logic. The new state gets tests: the exact message text, and
`testOnlyAPermissionRefusalCountsAsLocked` for the error classification. The on-screen checks
below are the evidence, against the local stack
(section 3a):
- wrong password;
- plain http with a **non-loopback** address, `http://192.168.1.50`:
  - without the switch, it is refused with "The server address must start with https://";
  - with the switch, it is allowed: it then fails to connect, and shows the iPhone
    local-network hint.

  Core always allows http to `localhost`, `127.0.0.1` and `::1`
  (`core/src/config.rs:46-49`), so the local stack cannot show the refusal;
- an address with a query is refused. An address with a path is accepted: the shared
  `ServerAddress` allows one by design (`https://host.lan/brook`) and refuses only credentials, a
  query or a fragment;
- a TOTP account: a code is asked, a wrong code shows the error, a recovery code works;
- quit and relaunch: still signed in (Done 4). At this point the placeholder signed-in view is
  enough.

All of these checks run in the simulator against the local stack. None uses chat.madalin.me.

Run: `clients/ios/build.sh test`; `clients/ios/build.sh run` (http to `127.0.0.1` needs no switch);
`BROOK_ALLOW_INSECURE_HTTP=1 clients/ios/build.sh run` for the "allowed" case.

If it stops here: you can sign in and stay signed in, but see no channels.

Docs made wrong: none.

### Step 7: channel list, live updates, foreground re-read, recovery warning, sign out

Subagent: `brook-apple-implementer`. Commit: `ios: channel list with live updates and sign out`.

Files:
- New `clients/ios/Brook/ChannelListView.swift`: a `NavigationStack` around a `List` of
  `channels.channels` in the model's order (channels, then DMs: `sidebarOrder`).
  - Each row shows `row.label`. While `channels.liveCalls[row.id]` is set, it also shows a call
    badge with the count, with `.accessibilityLabel("Call in progress, N participant(s)")`.
    Rows do not navigate.
  - `channels.error` takes the list's place.
  - `.refreshable { await channels.reloadList() }`.
  - When `recoveryCodesLeft <= 2`, one line above the list: "You have N recovery code(s) left."
    (the Mac's wording, pluralized the same way).
  - A toolbar Sign Out button opens a `confirmationDialog`, which calls `store.signOut()`.
  - `.task`, `.onDisappear` and `.onChange(of: scenePhase)` forward to the `SignedInSession`
    below (`start()`, `stop()`, `sceneChanged(from:to:)`).
  - No plain unread count: it comes from the Mac's local cache, which iOS does not have. The
    server's mention count shows.
  - In a `.lockedUntilFirstUnlock` launch, Sign Out shows `store.signOutNotice`, and sign-out sets
    it as `signOutWarning`, so the sign-in screen keeps it.
- New `clients/ios/Brook/SignedInSession.swift`: owns the per-session `ChannelsModel`, built once
  per sign-in with `me: user.id`, no notifier, the `AppActivity` default and
  `rereadOnReconnect: true`. The view only forwards `start`, `stop` and the scene change to it,
  so `SignedInSessionTests` can prove `stop()` cancels the subscription and each sign-in gets a
  fresh model.
- Shared `ChannelsModel`: `rereadOnReconnect` (default off). When on, any `ready` after the first
  re-reads the list: iOS has no cache to re-read after a reconnect. The Mac leaves it off; its
  `CacheFeed` re-reads. A flag, because the iOS client still conforms to `OfflineClient`.
- New `clients/ios/Brook/ForegroundReload.swift`:
  `extension ChannelsModel { func sceneChanged(from old: ScenePhase, to new: ScenePhase) async }`.
  It calls `reloadList()` when `new == .active && old != .active`. A comment says why: iOS
  suspends the socket in the background, and what changed meanwhile only shows after a re-read.
- `BrookApp.swift`: `.signedIn(user)` with a client shows `ChannelListView`.

Test first: `ForegroundReloadTests`, with `FakeRealtime`. After `start()`, `order` has one
`"list"`. `.background` → `.active` adds one. `.active` → `.inactive` adds none. `.inactive` →
`.active` adds one. Mutant: reload only on `.background` → `.active`. It must fail the
`.inactive` case, which is the real path out of suspension (`background` → `inactive` →
`active`). Add the test to `REQUIRED_TESTS`.

Run: `clients/ios/build.sh test`; `clients/macos/build.sh test`. The on-screen Done 1, 3, 4 and 5
checks, in the simulator against the local stack only (section 3a), with screenshots:
- a channel renamed, an account added and removed (the server API);
- a call started and ended (the harness);
- lock, end the call, unlock: no badge;
- sign out, then relaunch: the sign-in screen.

If it stops here: the app meets Done 2 to 7 in the simulator against the local stack. It still
lacks:
- Done 1's https server (chat.madalin.me) and the real iPhone;
- the README (Done 8).

Both come in step 8.

Docs made wrong: `clients/ios/README.md` (step 8).

### Step 8: README, then the owner's device check

Subagent: `brook-docs-writer`. Commit: `docs: build and run the iOS app; what it shares with the Mac`.
The spec and plan are brought up to date with steps 1 to 7 in the same PR.

- `clients/ios/README.md`:
  - build and test (`build.sh`, `build.sh test`, `build.sh run`);
  - a device run: create `Local.xcconfig` with the team, **run `clients/ios/build.sh` right
    before every Xcode device run**, open the generated `Brook.xcodeproj` in Xcode, choose the
    iPhone, then Run. Why: `build-xcframework.sh` deletes and rebuilds the xcframework each
    time, so a `clients/macos/build.sh` run since the last iOS build has left only the macOS
    slice, and the device build fails to link;
  - testing against the local stack (section 3a), with placeholders only, never credentials;
  - a table of what is shared (`clients/apple-shared`, `BrookCore`) and what is iOS-only (the
    views and the three seam files), and the seam rule from section 1.

  It keeps the existing push and CallKit notes, marked as later work.
- `clients/macos/README.md`: point to `clients/apple-shared` for the files that moved.
- `README.md` (repository layout): add `clients/apple-shared/`.

Then the owner's final check, the only one against chat.madalin.me (https), written into the
PR. The agent does not run it, because it cannot type real credentials into a non-local
server:
- The account: a testing account that the server side creates on chat.madalin.me at this
  point, on request. Its credentials never reach the repository or the agent.
- In the simulator (`clients/ios/build.sh run`) **and** on the iPhone (Xcode, after
  `clients/ios/build.sh`): Done 1 (sign in, channels and DMs), Done 4 (still signed in after a
  quit) and Done 5 (sign out, then relaunch).
- On the iPhone: Done 2 (two-factor, with the owner's help) and the Done 3 lock and unlock
  badge check.
- On the iPhone, the pre-unlock path, which the simulator cannot produce (it does not enforce
  file protection):
  - restart the iPhone and open Brook before unlocking it: it must not restore, and must show
    the unlock message;
  - restart, unlock, then open Brook: it must restore. This also checks that a prewarm before
    the first unlock does not leave the process off;
  - which error a locked device returns for the probe (`isProtectionRefusal` assumes a
    permission error). The safety does not depend on it, only the message does.
- On the iPhone: type a recovery code, then open the app switcher. The app's card must not show
  the code.

This is where the iOS TLS path (rustls, certificate verification, a real handshake) runs for
the first time. Until then the only network test is step 1's `brook.invalid` smoke test, which
gets as far as a DNS failure but no handshake; every agent-run check is plain http to
loopback. A TLS failure found here goes back to step 1's area (`brook-core-implementer` if it
is crate-side).

## 3. Migrations and production

No server, protocol, database or core change. The Mac's local data and Keychain items are
untouched: same service, same group, same slots, and the Mac never calls `deleteAll`. The iOS
app is new, so it has no data to migrate. To roll back, revert the commits. Step 2's fix is
independent of the others and can stay on its own.

### 3a. A local Brook server on this Mac, for the simulator checks

It runs: Docker Desktop is installed on this Mac (Docker 29.8, Compose v5.5), and `deploy/`
needs nothing else. No new script; the existing Makefile does it, from `deploy/`:

| Purpose | Command |
|---|---|
| First time: create the gitignored `deploy/.env` with random secrets | `make init` |
| Bring up postgres, api and Caddy on `http://127.0.0.1:8080` (loopback only, the compose default) | `make up` |
| Calls (the call-badge checks): Janus | `make media` |
| End the session, keep the data | `make down` |
| Wipe all accounts and data | `docker compose down -v` |

- Calls also need `BROOK_DEV_HARNESS=true` in `deploy/.env`, for the browser call harness at
  `http://127.0.0.1:8080/dev/call`, which serves as the "other client" that starts and ends a
  call. Only in this local `.env`.
- The simulator shares the Mac's network, so `http://127.0.0.1:8080` reaches the stack from the
  app. Loopback is not a LAN address, so there is no local-network prompt.
- Plain http to the local stack needs **no switch**: core accepts `http://` for `localhost`,
  `127.0.0.1` and `::1` by itself (`core/src/config.rs:46-49`). Enter
  `http://127.0.0.1:8080`, and no insecure warning shows.
- The switch matters only for a non-loopback http address. It is used in the step 6 refusal
  check with `http://192.168.1.50`, and is set either way:
  - `BROOK_ALLOW_INSECURE_HTTP=1 clients/ios/build.sh run`, which passes
    `SIMCTL_CHILD_BROOK_ALLOW_INSECURE_HTTP=1` to `simctl launch`. It is good for that launch
    only; a launch from the home screen does not have it;
  - `xcrun simctl spawn booted defaults write me.madalin.brook AllowInsecureHTTP -bool YES`.
    It persists in that simulator; undo it with `defaults delete`. Both are read by `Settings`,
    and the form shows the warning.
- Every agent-run simulator check uses this local stack. chat.madalin.me is used only in the
  owner's final check (step 8).
- Throwaway accounts:
  - the first `POST /api/v1/auth/register` on an empty stack becomes the admin (as in
    `deploy/README.md`). Later accounts are registered by that admin with `admin_password`;
  - passwords are generated at run time (`openssl rand -base64 18`) into a mode-600 file
    outside the repository (the session scratchpad), and are never printed or passed on a
    command line. JSON bodies and the bearer token go through files, as
    `provision-itest-account.sh` does;
  - TOTP for one throwaway account: `POST /auth/totp/enroll` and then `/activate`, with codes
    computed by `python3` from the enrollment's secret (standard library `hmac`, no new
    dependency);
  - "another client" for rename, add and remove is the API itself (`PATCH /channels/{id}`,
    add or remove a member), called with the admin's token.
- Do not point the installed Mac app at the local stack: it shares the owner's bundle id,
  defaults and Keychain, and the owner uses it against chat.madalin.me.
- Automated integration tests stay as they are: `bindings/apple/itest.sh` with the gitignored
  `bindings/apple/.itest.env`. This plan adds none, and no credentials go into the repository.
- Worktrees: the compose project is named `brook`, so run the stack from one checkout at a time.

## 4. Risks

- **The core may link but fail at run time in the simulator.** Step 1 exists to find this first.
  It proves, in the simulator:
  - the app binary links and calls Rust on screen;
  - the tokio runtime runs and an async future crosses UniFFI;
  - DNS resolution runs (`brook.invalid` fails with `Network`);
  - a Rust-to-Swift callback (`FfiKeySlot.load` during `restore`) works.

  It does **not** prove TLS: no handshake, no certificate verification. A link error naming a
  framework is fixed with `OTHER_LDFLAGS` in `project.yml`. A crate-side fix goes to
  `brook-core-implementer` (step 1b).
- **TLS on iOS is first exercised in the owner's final check (step 8)** against chat.madalin.me.
  Every agent-run check is plain http to loopback. If the handshake or certificate verification
  fails on iOS, it surfaces only at the end, and goes back to step 1's area.
- **A device build after a Mac build** links against a macOS-only xcframework and fails.
  `build-xcframework.sh` deletes and rebuilds the xcframework on each run. The README says to
  run `clients/ios/build.sh` before each Xcode device run. Building into separate per-app
  outputs would remove this, at the cost of a second output tree; not done.
- **The simulator Keychain with no team set.** If the `probe` returns `-34018` in a simulator
  run, persistence is off and Done 4 fails there. Check it on screen in step 6. If it happens,
  sign simulator runs with the team from `Local.xcconfig`. Do not add an entitlement.
- **The badge after unlock depends on the socket coming back quickly.** The foreground re-read
  refreshes the rows, but `liveCalls` is cleared only by the next `ready`. If iOS hands core a
  half-open socket, detection waits for core's 45 s idle timeout plus backoff. Step 7's
  lock and unlock check measures it. If it is more than a few seconds, that is a core change ("reconnect
  now" on foreground); it goes back to the owner, not into this plan.
- **A Message constant added later is not in the wording test's hand-made list.** Swift cannot
  enumerate static lets. The test's comment says to add each new constant, and review checks it.
- **The lock serializes Mac and iOS builds.** A Mac `build.sh install` waits for a running iOS
  test run. `itest.sh` calls `build-xcframework.sh` without the lock; running it at the same time
  as a `build.sh` can still race. Rare, and left as is.
- **The shared tests run twice**, once per app: about 30 s more per iOS run. Accepted: that is
  how the shared code is proven on both platforms.
- **Package resolution for iOS** still downloads the `WebRTC` binary target (it is in the same
  package) but does not build it. If Xcode does try to build `BrookMedia` for iOS, depend on the
  product by name only, and check the generated scheme.
- **The data directory is in Application Support**, which iOS backs up. Settled in step 5's
  security review: it stays in backups, on purpose. An encrypted backup restored to the same
  device brings back the Keychain item (this device only); excluding the fences would restore a
  session without its fence. A comment beside `live()` says so.

## 5. Decisions for the owner

1. **Sharing through `clients/apple-shared/` (`Brook/`, `BrookTests/`)**, compiled into each app
   (section 1), rather than through `BrookCore`. The name and the place are open to change.
2. **The xcframework slices follow the caller** (the Mac builds only macOS), and both `build.sh`
   scripts wait on one `lockf` lock instead of racing.
3. **iOS wording** of the device-specific messages: "If your iPhone asked to allow local network
   access, allow it and try again." and "This iPhone couldn't forget your saved sign-in…".
4. **Device runs from Xcode, not from `build.sh`.** `build.sh` covers build, test and simulator
   run; the README gives the Xcode steps for the iPhone.
5. **The first-launch cleanup deletes every item under the service**, not only `session:*`
   slots. On iOS there is no local data, so they are the same set today. Whether a reinstall
   should ever keep a Keychain item is the owner's call.
