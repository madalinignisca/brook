# Plan: iOS joins and rings for calls, phase 1 (#355)

Spec: [2026-10-10-ios-calls-spec.md](2026-10-10-ios-calls-spec.md) (approved; §8 decided).
Review: `brook-reviewer` for every step. No step touches authentication or authorization, so
`auth-reviewer` is not needed. Step 4 is the only server change: a dev-only page and its e2e
script, off in production.

Changed after the first review: the engine starts the camera only once the app arms it, so a
refused or rolled-back `call.media` cannot start it (step 3); the permission is checked only when
the join deferred the camera, so the Mac is unchanged (step 3); the brief camera-on announcement
at join is accepted, and its timing check is dropped (risk 1); `JoinPlan.resolve` has no default
settings text; `CallModelTests.swift` loses `import AppKit`; the camera pause goes through
`SignedInSession.sceneChanged`; the debug statistics live in `PhoneCalls`; a refused or
listen-only mute is handled on purpose; the ring ends with a reason; the e2e script uses
`?tone=1`; the self-view is mirrored; #293's rebase is a hand edit (risk 6).

Phase 2 (ringing in the background or closed, PushKit and APNs) is not in this plan. It gets its
own issue, spec and plan once this phase is merged (spec §8, question 1).

## 1. Approach

Nine steps. Step 0 is a spike with no pull request; each later step is one pull request that
leaves `main` green and shippable:

0. The CallKit spike in the simulator. Its result decides how the [sim] items are checked.
1. `BrookMedia` and WebRTC build and test on the iOS simulator.
2. The Mac's call logic moves to `clients/apple-shared`. Mac behavior does not change.
3. The camera starts off at join, behind a flag that is off on the Mac.
4. The dev call harness counts audio energy and can send a tone (server).
5. iOS: the audio session, CallKit, background audio and permissions, with no screen yet.
6. iOS: the call button, the call screen and the return bar. The simulator check of Done 1 to
   3, 5, 6, 10 and 11.
7. iOS: the DM ring while Brook is open, once #293 has merged.
8. The owner's check on a real iPhone.

Step 4 depends on nothing else and can run beside steps 1 to 3. It must merge before step 6's
simulator check. Each step's pull request carries the doc edits it makes necessary, written by
`brook-docs-writer` (and by `brook-spec-writer` for the spec's "as built" notes).

**Sharing: move the files, do not copy them** (as in the conversation plan). Both
`project.yml` files already compile `../apple-shared/Brook` and `../apple-shared/BrookTests`.
Files move with `git mv`. What cannot build on iOS is cut out first, while the files are still
Mac-only.

**The quit handshake: the Mac reads it from `CallCenter`.** Today `CallCenter` owns a
`QuitCoordinator` (AppKit) and sets its `leaveActiveCall`. After the move, `CallCenter` keeps the
same closure in its own `private(set) var leaveForQuit`, set and cleared on the same lines.
`QuitCoordinator` stays on the Mac and reads that closure through an init argument when ⌘Q
arrives. Alternative considered: a seam type each app defines. iOS has no quit handshake, so it
would define an empty type for nothing.

**Per-platform text: `ThisDevice`, as for the session messages.** Shared text names
`ThisDevice.name` and two new facts: `privacySettings` ("System Settings › Privacy & Security"
on the Mac, "Settings › Privacy & Security" on iOS) and `elsewhere` ("another window or device"
on the Mac, "another device" on iOS). `JoinPlan` lives in `BrookMedia`, which cannot see
`ThisDevice`, so `JoinPlan.resolve` takes the settings path as a required argument (no default: each
caller says which device it words for). The Mac's text stays byte for byte the same, and a test holds it to the literals.

**Camera off at join (spec §8, question 3): one flag, off on the Mac.** `CallCenter(cameraAtJoin:)`
defaults to `true`. With `false`:
- `JoinPlan.resolve` never asks for the camera;
- the engine is built with `MediaOptions(cameraOn: false)`, so it keeps the camera track and its
  m-line but never starts capture until the app arms the camera (`armCamera()`, from the first
  camera tap). Core rolls a refused `call.media` back to its default of camera on
  (`core/src/call.rs`, `set_media` and the `MediaAck` rollback). Without the arm, that rollback
  would start the camera while Brook shows it off. With it, the rollback only sets the intent;
- `CallModel.start()` sends `setMedia(video: false)` at once, because the server assumes a
  published camera is wanted on (`services/api/app/calls.py`, `video_intent = True`);
- the first camera tap asks for the permission. Only a join that deferred the camera asks. The
  Mac never defers, so its path is unchanged.

Alternatives considered:
- *No camera m-line at join, added with a republish at the first tap.* This is new engine work.
  It also loses what the Mac has: the camera turns on and off without renegotiating.
- *An initial media intent on core's `join_call`.* It would remove the rollback to camera on and
  the brief camera-on announcement (risk 1). Rejected: it changes core and the FFI, which GNOME
  and KDE build, and the spec allows no core change in this phase.

**CallKit: one iOS coordinator behind a small protocol.** `PhoneCalls` (iOS, `@MainActor`) owns
the mapping from Brook call to system call (`UUID`) and drives the shared `CallCenter`. It
reaches CallKit only through a protocol, `SystemCalls`. `CallKitCalls` is the real
implementation and a fake stands in for it in the tests. Every user action goes through the
system, Brook's own buttons included: Brook's mute asks CallKit for a `CXSetMutedCallAction`,
and the provider's `perform` sets `CallModel`. So Brook's button and the system screen are one
path, and the two cannot drift apart. Alternative considered: Brook's buttons set `CallModel`
directly and report to CallKit afterwards. That makes two paths, and the mute state can differ
between them.

**Audio: WebRTC's manual audio, switched on by CallKit.** On iOS, WebRTC's default audio device
already uses Voice Processing I/O (echo cancellation). `RTCAudioSession.useManualAudio = true`
keeps it silent until CallKit's `provider(_:didActivate:)` sets `isAudioEnabled = true`. This is
WebRTC's documented CallKit pattern. Alternative considered: a custom `RTCAudioDevice`. It is
only needed if echo shows up on the device (step 8). It would go to the owner first, as on the
Mac.

**The ring rule is #293's, moved, not rewritten.** `IncomingCalls` moves to shared code. Its
`Ringer` (the Mac's sound) gains the ringing call as an argument. The iOS `Ringer` is CallKit:
`start` reports the incoming call and `stop` ends it.

## 2. Steps

Every step:
- is one pull request whose commits are made with `git commit -s` by `brook-committer`;
- starts each new file with the two SPDX lines;
- leaves `clients/ios/build.sh test` and `clients/macos/build.sh test` green. From step 1 on,
  the iOS command also runs the engine tests on the simulator;
- keeps a new test only after it has been seen failing against the mutant the step names. The
  failing output is pasted into the pull request.

Commit prefixes: `apple:` (shared Swift, `BrookMedia`, Apple scripts), `mac:`, `ios:`, `api:`
(the harness), `docs:`.

Commands (from the repository root unless said otherwise):
- iOS: `clients/ios/build.sh test`.
- Mac: `clients/macos/build.sh test`. "The Mac's count" is both numbers at its end: XCTest's
  `Executed N tests` and Swift Testing's `Test run with N tests`.
- Engine on macOS: `bindings/apple/build-xcframework.sh && (cd bindings/apple/swift/BrookCore && swift test --filter BrookMediaTests --skip LiveCallTests)`.
  Never run this while a `build.sh` runs: the script deletes the xcframework that `build.sh`
  reads, and only `build.sh` takes the lock.
- API (from `services/api`, after `uv sync --locked --extra dev`): the CLAUDE.md §3 row.

**`REQUIRED_TESTS`** (`clients/ios/build.sh`) lists only XCTest tests, matched as
`Test Case '-[Module.Class name]' passed`. Every new test in this plan is an XCTest, and each
step names what it adds.

### Step 0: the CallKit spike (no pull request)

Subagent: `brook-apple-implementer`. It writes a throwaway app in the session's scratchpad, never
in the repository: XcodeGen, one SwiftUI view, a `CXProvider` and a `CXCallController`, with
`NSMicrophoneUsageDescription` and `UIBackgroundModes: [audio]`. It runs the app on the "iPhone
17" simulator and logs, with timestamps:
- **A. Outgoing:** a `CXStartCallAction` request completes without an error, and the provider's
  `perform(CXStartCallAction)` runs.
- **B. Audio:** after `action.fulfill()`, `provider(_:didActivate:)` runs within 5 s, and an
  `AVAudioEngine` input tap then reads non-zero samples from the Mac's microphone while
  `afplay /System/Library/Sounds/Submarine.aiff` plays. This needs macOS microphone permission
  for Simulator, a one-time step for the owner (spec Done 2).
- **C. Incoming:** `reportNewIncomingCall` completes with no error, and a screenshot
  (`xcrun simctl io booted screenshot`) shows whether the system's incoming-call screen appears.

The log and the screenshot go into a comment on #355. The result also goes into this plan as an
"as built" note, written by `brook-plan-writer`.

What changes with the result:

| Result | Changes |
|---|---|
| A, B and C pass | The plan as written. Every [sim] item goes through CallKit. |
| A or B fails | Step 5 also builds the debug-only direct audio path (step 5, part b). Items 1 to 3, 5, 6, 10 and 11 are checked in the simulator through it. CallKit wiring, the audio hand-over and the background (Done 7) are checked on the device only (step 8). |
| C fails | Done 9 is checked on the device only. Its unit tests still run in `build.sh test`. |
| B's samples are zero but `didActivate` runs | Done 2's phone-to-harness direction is checked on the device only. The harness-to-phone direction still runs in the simulator. |

### Step 1: `BrookMedia` builds and tests on iOS

Subagent: `brook-apple-implementer`. Commit: `apple: build and test the call engine on iOS`.

Files:
- `bindings/apple/swift/BrookCore/Sources/BrookMedia/ScreenCapture.swift`: the whole file inside
  `#if os(macOS)` … `#endif`, imports included. Add a comment: ScreenCaptureKit is macOS only,
  and sharing the iPhone's screen is out of scope (spec §8, question 2). Viewing a shared screen
  needs no capture, so nothing else changes.
- `bindings/apple/swift/BrookCore/Tests/BrookMediaTests/ScreenCaptureTests.swift`: the same guard.
- `bindings/apple/swift/BrookCore/Package.swift`: the `platforms` comment no longer says that
  BrookMedia is macOS only. The WebRTC binary target already carries iOS slices.
- `clients/ios/project.yml`: no change yet. The app links `BrookMedia` in step 2, when it first
  uses it.
- `clients/ios/build.sh`, `test` mode: a new stage 5 after the app tests. It runs from
  `bindings/apple/swift/BrookCore`:
  `xcodebuild test -scheme BrookCore-Package -destination "$sim_dest" -derivedDataPath "$derived/media" -only-testing:BrookMediaTests -skip-testing:BrookMediaTests/LiveCallTests -collect-test-diagnostics never`,
  tee'd to its own log. It then checks a second array, `MEDIA_REQUIRED_TESTS`, with the same
  `grep`. The array holds every `func test` in `BrookMediaTests` except `LiveCallTests` and
  `ScreenCaptureTests`, by class and name (about 31 of them). A comment says that the
  xcframework already has the simulator slice, because the script runs
  `build-xcframework.sh --ios` before any stage.

Test first: the guard has nothing to unit-test. The failing state to watch: before the guards,
stage 5 fails to compile (`no such module 'ScreenCaptureKit'`). Paste it.

Mutant: in stage 5, change `-only-testing:BrookMediaTests` to `-only-testing:BrookMediaTests/OfferProbeTests`.
`build.sh test` must then exit non-zero, naming a `LoopbackTests` test that did not run. This
proves the array is checked.

Run: `clients/ios/build.sh test` (engine tests included), `clients/macos/build.sh test` (the Mac's
count is unchanged), and the macOS engine command (`ScreenCaptureTests` still runs on the Mac).

Docs: `clients/ios/README.md`, "Build and test": `build.sh test` also runs the call engine's
tests on the simulator. "Later work › Calls": `BrookMedia` builds for iOS, and the app does not
use it yet.

If it stops here: the iOS app is unchanged. Only the tests build `BrookMedia` for iOS.

### Step 2: the call logic moves to `clients/apple-shared`

Subagents: `brook-apple-implementer` for commits 1 to 3, `brook-docs-writer` for commit 4.

1. `apple: the call logic no longer reaches AppKit or names the Mac` (the app files still in
   `clients/macos`, plus `JoinPlan` in `BrookMedia`):
   - `Brook/Calls/CallCenter.swift`: remove the `quit` property and init argument. Add
     `private(set) var leaveForQuit: (@MainActor () async -> Void)?`, set where
     `quit.leaveActiveCall` is set today and cleared where it is cleared. Add a comment: the
     Mac's quit handshake reads it, and iOS has none.
   - `Brook/Calls/QuitCoordinator.swift`: `leaveActiveCall` becomes a read of a new init argument,
     `activeCall: @escaping @MainActor () -> (() async -> Void)? = { nil }`, made when
     `shouldTerminate()` runs. The rest is unchanged.
   - `Brook/BrookApp.swift`: `appDelegate.quit = QuitCoordinator(activeCall: { [calls] in calls.leaveForQuit })`.
   - `Brook/ThisDevice.swift`: `privacySettings = "System Settings › Privacy & Security"`,
     `elsewhere = "another window or device"`.
   - `Brook/Calls/CallModel.swift`, `endedText`: `.engineFailed` becomes
     `"Media stopped working on this \(ThisDevice.name)."`, and `.replaced` becomes
     `"You joined this call from \(ThisDevice.elsewhere)."`.
   - `CallCenter.performJoin`: `JoinPlan.resolve(auth, settings: ThisDevice.privacySettings)`.
   - `bindings/apple/.../BrookMedia/Capture.swift`: `micDenied` and `cameraDenied` become
     `static func micDenied(settings: String) -> String` (and `cameraDenied` the same way),
     interpolating `settings` where the path is today. `resolve(_:settings:)` passes it on. No
     default on any of them: `CallCenter` passes `ThisDevice.privacySettings`, and
     `CaptureTests.swift` passes a test string and compares against the function with the same
     string. `CallModelTests.swift` and `ScreenshotRenderer.swift` pass
     `ThisDevice.privacySettings`.
   - `BrookTests/CallModelTests.swift`: remove `import AppKit` (line 5; nothing in the file uses
     it, and it cannot build on iOS). Cut `QuitCoordinatorTests` and
     `CallReviewFixTests.testQuitDuringJoinWaitsForTheJoin` into a new Mac-only
     `BrookTests/QuitCoordinatorTests.swift`, adapted to `QuitCoordinator(activeCall:)` (the
     mid-join test passes `{ center.leaveForQuit }`).
2. `apple: share the call logic with iOS`: only `git mv`, every file byte-identical, plus the two
   lines iOS needs to build them:
   - `clients/macos/Brook/Calls/{CallModel,CallStage,CallCenter}.swift` → `clients/apple-shared/Brook/Calls/`;
   - `clients/macos/BrookTests/{CallModelTests,CallStageTests}.swift` → `clients/apple-shared/BrookTests/`;
   - `clients/ios/project.yml`: the app links `products: [BrookCore, BrookMedia]`. Its comment
     now says that the calls use the engine;
   - `clients/ios/Brook/ThisDevice.swift`: `privacySettings = "Settings › Privacy & Security"`,
     `elsewhere = "another device"`.
   `CallViews.swift`, `ScreenPicker.swift` and `QuitCoordinator.swift` stay on the Mac.
3. `ios: require the moved call tests`: `REQUIRED_TESTS` gains every `func test` in the two moved
   files, by class and name. The reviewer checks the count against
   `grep -c 'func test'`. `MessageWordingTests.messages` gains `CallModel.endedText(r)` for every
   `FfiEndReason`, `JoinPlan.micDenied(settings: ThisDevice.privacySettings)` and the
   `cameraDenied` twin.
4. `docs: the call logic is shared`: `clients/ios/README.md` ("What is shared": `CallModel`,
   `CallStage`, `CallCenter`; the Mac keeps the call window, the screen picker and the quit
   handshake) and `clients/macos/README.md` ("Shape").

Tests first (commit 1), Mac-only, in `BrookTests/ThisDeviceTests.swift`:
`testTheMacWordingIsUnchanged` gains `CallModel.endedText(.engineFailed)`,
`CallModel.endedText(.replaced)`, `JoinPlan.micDenied(settings: ThisDevice.privacySettings)` and
`JoinPlan.cameraDenied(settings: ThisDevice.privacySettings)`, each pinned to its literal on
`main` today. Mutant: interpolate `ThisDevice.system` for `ThisDevice.name`. The test must fail.
The moved quit tests are the proof for the handshake. Mutant: `QuitCoordinator` reads
`activeCall` once, at init. `testQuitDuringJoinWaitsForTheJoin` must fail.

Run after each commit: `clients/macos/build.sh test`. The Mac's count stays the same, because the
moved tests still compile into the Mac. After commit 2 and after commit 3:
`clients/ios/build.sh test`. Record `du -sh` of the simulator `Brook.app` before and after commit
2 in the pull request (risk 3).

If it stops here: the iOS app links WebRTC and is larger, but it shows nothing new. The Mac is
unchanged, which the pinned text and the unchanged test count show.

### Step 3: the camera starts off at join, on iOS only

Subagent: `brook-apple-implementer`. Commit: `apple: a join can start with the camera off`.

Files:
- `BrookMedia/WebRTCEngine.swift`:
  - `MediaOptions` gains `cameraOn: Bool = true`, and `EngineCore.media` starts as
    `(audio: true, video: options.cameraOn)`. Comment: off keeps the camera's track and m-line,
    so turning it on needs no renegotiation (`syncCapture`).
  - `EngineCore` gains `cameraArmed`, which starts as `options.cameraOn`, so it is always armed
    on the Mac. The capture goal is `media.video && cameraArmed`. `syncCapture`'s loop
    condition itself compares that goal with `captureRunning` (today
    `media.video != captureRunning`, WebRTCEngine.swift:636), and so does the branch that picks
    start or stop. Guarding only the start branch would make an unarmed engine with
    `media.video` true loop forever.
  - `WebRTCEngine.armCamera()` enqueues `cameraArmed = true`. It never disarms. Comment: core
    rolls a refused `call.media` back to camera on, and only the user's tap may start the camera.
  - `CallMedia` (in `CallModel.swift`) gains `armCamera()`. `WebRTCEngine` already conforms, and
    the shared `FakeMedia` records the calls.
- `BrookMedia/Capture.swift`:
  - `JoinPlan` gains `public let cameraOn: Bool`. Its doc: "the camera runs from the start; false
    means a track is published and stays off until the first tap". The init gains
    `cameraOn: Bool? = nil` (nil means `camera`), so every existing call is unchanged.
  - `resolve(_:settings:cameraAtJoin: Bool = true)`. With `false`, the microphone is resolved as
    today, and the camera is never requested. A camera already `.denied` gives
    `JoinPlan(microphone: true, camera: false, explanation: cameraDenied(settings:))`. Anything
    else gives `JoinPlan(microphone: true, camera: true, explanation: nil, cameraOn: false)`.
  - `CameraCapture.start`: on iOS, the front camera:
    `AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: .front)` inside
    `#if os(iOS)`. `default(for: .video)` is the back camera on an iPhone. This is a package
    file, not a shared app file, so the seam rule does not apply.
- `apple-shared/Brook/Calls/CallCenter.swift`: `init(auth:cameraAtJoin: Bool = true, makeEngine:)`.
  `liveEngine` passes `cameraOn: plan.cameraOn`. The join passes `cameraAtJoin` to `resolve`, and
  `auth` to `CallModel`.
- `apple-shared/Brook/Calls/CallModel.swift`:
  - The init gains `auth: AuthorizationSource = SystemAuthorization()`.
    `cameraOn = plan.cameraOn` and `confirmed = (plan.microphone, plan.cameraOn)`.
  - At the end of `start()`: `if plan.camera && !plan.cameraOn { await setMedia(audio: micOn, video: false) }`.
    The comment cites `calls.py`'s `video_intent = True`.
  - `cameraDeferred = plan.camera && !plan.cameraOn`, fixed at init. It is false on the Mac.
  - `toggleCamera()`, when turning on with `cameraDeferred` and not yet armed: `.notDetermined`
    asks `auth.request(.video)`, and `.granted` arms at once (`media.armCamera()`, then
    `setMedia`). A refusal, or a status that is already `.denied`, sets
    `cameraProblem = "Brook can't use your camera. Allow it in \(ThisDevice.privacySettings) › Camera, then turn it on again."`
    (a new `static func cameraDenied`, added to `MessageWordingTests.messages`) and returns
    without `setMedia`. Without `cameraDeferred` (the Mac), `toggleCamera` reads no permission
    and works as today.
  - A refused first `setMedia` (the camera-off announcement): `confirmed` is already
    `(micOn, false)`, so the UI keeps showing the camera off. The engine is not armed, so
    core's rollback to camera on starts no capture. The server keeps its default (camera
    announced on, no frames) until the next accepted `call.media`, which any mute or camera
    toggle sends. Nothing retries, as for any refused toggle on the Mac. This `setMedia` can
    go out with an empty call id. `join_call` returns once the server confirmed the join, but
    the call task sets its id only when it handles `call.joined` (`core/src/call.rs`,
    `on_joined`), and its `select!` is not `biased`. So the input can be handled first. That is
    unlikely, and with the arm it is harmless: the server refuses it, the camera stays off, and
    the server announces the camera on with no frames until the next accepted `call.media`.
    `biased;` in core's `select!` would remove the race. It is a possible later fix and is not
    needed now.
  - New `func setMic(_ on: Bool) async`, which `toggleMic` calls. CallKit's mute (step 5) sets a
    value, it does not flip one.

Test support first, in `clients/apple-shared/BrookTests/TestSupport.swift`: `FakeCallHandle`
records `setMedia` calls as `FakeHandle` does (`media`, as `"audio:video"` strings), and
`FakeRealtime` records each `joinCall` (`joins: [(channelId, publish)]`) and keeps the last handle
it returned (`lastHandle`). Steps 5 and 7 use both.

Tests first:
- `CaptureTests.swift`, `CameraToggleTests.testACameraThatStartsOffIsNotStartedByTheOffer`: an
  engine with `cameraOn: false` and a `ScriptedCapture`. The offer contains `m=video` and
  `starts == 0`. Mutant: ignore `options.cameraOn`. The test must fail.
- `CameraToggleTests.testAnUnarmedCameraIgnoresTurningOn`: `cameraOn: false`, then
  `setLocalMedia(audio: true, video: true)` without arming (this is what core's rollback does).
  `starts == 0` after 300 ms. Then `armCamera()` and `setLocalMedia(true, true)` bring `starts`
  to 1. Mutant: drop the `cameraArmed` check in `syncCapture`. The test must fail.
- `JoinPlanTests.testWithoutTheCameraAtJoinOnlyTheMicrophoneIsAsked` (`asked == [.audio]`,
  `cameraOn == false`, `camera == true`) and `testWithoutTheCameraAtJoinADeniedCameraIsSaid`.
  Mutant: `resolve` ignores `cameraAtJoin`. The first test must fail.
- Shared `CallModelTests`, new class `CameraAtJoinTests`:
  `testACallThatStartsWithTheCameraOffAnnouncesIt` (`FakeHandle.media == ["true:false"]` after
  `start()`); `testTheFirstCameraTapAsksArmsAndTurnsItOn` (one request, one `armCamera`, then
  `"true:true"`); `testADeniedCameraSaysSoAndStaysOff` (no `setMedia`, no arm, `cameraProblem`
  set); `testARefusedCameraOffAnnouncementKeepsTheCameraOff` (`FakeHandle(refuse: true)`:
  `cameraOn` stays false and `armCamera` is never called); `testSetMicSetsAValue`. Mutant: drop
  the `setMedia` in `start()`. The first test must fail.
  These tests use a new shared `ScriptedAuth: AuthorizationSource` that records requests, never
  `SystemAuthorization`. The existing tests that build a `CallModel` with the default `auth`
  (such as `testCameraProblemTurnsTheCameraOffAndKeepsTheCall`) use plans without
  `cameraDeferred`, so they never reach the system permission.
- Mac-only `BrookTests/MacCallTests.swift`: `testTheMacJoinsWithTheCameraOn`. A `CallCenter()`
  with a granted fake auth and an engine factory that records the plan. A `FakeRealtime` with an
  open `joinGate`. The plan has `cameraOn == true`, the video was requested at join, and
  `client.lastHandle.media` stays empty after the join. Mutant: make the `cameraAtJoin` default
  `false`. The test must fail.
- The same file: `testTheMacTogglesTheCameraWithoutAsking`. A `CallModel` with the full plan and
  a `ScriptedAuth` whose camera status is `.denied`. Toggling off and on again gives
  `["true:false", "true:true"]`, with no status read or request, and `armCamera` is never called
  (the engine is armed from the start). Mutant: check the permission on every turn-on. The test
  must fail.

`REQUIRED_TESTS` gains the five `CameraAtJoinTests`. `MEDIA_REQUIRED_TESTS` gains the four
`BrookMedia` tests.

Run: the macOS engine command, `clients/macos/build.sh test`, `clients/ios/build.sh test`.

If it stops here: no app uses `cameraAtJoin: false` yet, and the Mac test pins the Mac's
behavior.

### Step 4: the dev harness counts audio energy (server)

Subagent: `brook-server-implementer`. Commit: `api: the call harness counts audio and sends a tone`.

Files:
- `services/api/app/static/call_harness.html`:
  - `window.brook.stats()` gains `inboundAudioEnergy`: the sum of `totalAudioEnergy` over
    `inbound-rtp` reports with `kind === "audio"` on the subscribe connection. Comment: bytes
    flow even for silence and mute, while energy rises only for sound.
  - `?tone=1`: the published audio is a 440 Hz `OscillatorNode` through a
    `MediaStreamAudioDestinationNode`, instead of the microphone. This gives a steady source
    without a fake device.
  - `window.brook.setMedia(audio, video)`: sets `enabled` on the local tracks and sends
    `call.media` (PROTOCOL.md §3). This is how the harness mutes.
- `services/api/e2e/call_e2e.py`:
  - `CHROME` is read from `BROOK_E2E_CHROME`, defaulting to today's path, so the script runs on
    the Mac (`/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`).
  - The first page loads with `&tone=1`, so the tone source that step 6 relies on is tested.
    The second keeps Chrome's fake microphone, which beeps.
  - After the video checks: both pages' `inboundAudioEnergy` rises over 2 s. Then the first page
    calls `setMedia(false, true)`, and the second page's energy rises by less than 1% of what it
    rose by before. Then `setMedia(true, true)` makes it rise again.

Test: the e2e script is the test. It needs a fresh stack. Before the iOS checks, from `deploy/`:
`make reset && make media` with `BROOK_DEV_HARNESS=true` in the local `.env`. Then, from
`services/api`:
`BROOK_E2E_CHROME=… uv run --with playwright --with httpx python e2e/call_e2e.py http://127.0.0.1:8080`.
Mutants, each watched failing: count `inbound-rtp` audio `bytesReceived` instead of energy (the
mute assertion fails); make `setMedia` send `call.media` without disabling the track (the mute
assertion fails); set the tone's gain to 0 (the second page's energy check fails). Then the API row of CLAUDE.md §3 (ruff covers `e2e/`).

Docs: none. The harness is dev-only and the iOS README already names it. PROTOCOL.md does not
move.

If it stops here: a dev-only page has two more functions. Production is unchanged
(`dev_harness` is off by default).

### Step 5: the audio session, CallKit, background audio and permissions

Subagent: `brook-apple-implementer`. Commits: `apple: hand the iOS call audio to the system`
(the `BrookMedia` part), then `ios: calls go through CallKit`.

Files:
- New `BrookMedia/CallAudio.swift`, all inside `#if os(iOS)`. `public enum CallAudio` has:
  - `prepare()`, called once at launch: `RTCAudioSession.sharedInstance()` gets
    `useManualAudio = true` and `isAudioEnabled = false`. Its configuration (through
    `RTCAudioSessionConfiguration.webRTC()`) is `.playAndRecord`, mode `.videoChat` (the speaker
    unless headphones or Bluetooth are connected, spec §8 question 4), and options
    `[.allowBluetoothHFP, .allowBluetoothA2DP]`. Use the iOS 26 names: deprecated ones fail
    under warnings-as-errors.
  - `activated(_ session: AVAudioSession)`: `audioSessionDidActivate` and `isAudioEnabled = true`.
  - `deactivated(_:)`: the reverse.
  Comment: WebRTC must not start the audio unit before CallKit hands the session over, or iOS
  refuses it on a locked phone.
- New `clients/ios/Brook/Calls/SystemCalls.swift`: `@MainActor protocol SystemCalls: AnyObject`,
  with `requestStart(_ id: UUID, title: String) async throws`, `reportConnected(_ id: UUID)`,
  `reportIncoming(_ id: UUID, caller: String) async throws`,
  `reportEnded(_ id: UUID, reason: CXCallEndedReason)`, `requestEnd(_ id: UUID) async throws`
  and `requestMute(_ id: UUID, _ muted: Bool) async throws`.
- New `clients/ios/Brook/Calls/CallKitCalls.swift`: `CXProvider` and `CXCallController`, created
  once per process (Apple: one provider per app). The configuration: `supportsVideo = true`;
  `maximumCallGroups = 1` and `maximumCallsPerCallGroup = 1` (one call: a second ring offers
  "End & Accept", spec §6); `includesCallsInRecents = false`; handle type `.generic`; no
  ringtone, so the system's is used. Each `CXCallUpdate` sets `hasVideo = true`,
  `localizedCallerName` (the title), and `supportsHolding`, `supportsGrouping`,
  `supportsUngrouping` and `supportsDTMF` to false. The `CXProviderDelegate` forwards
  start, answer, end, mute, `didActivate` and `didDeactivate` to a `weak var phone: PhoneCalls?`,
  and fulfills or fails each action with what `PhoneCalls` returns. The delegate is set on the
  main queue (`setDelegate(self, queue: nil)`), because `PhoneCalls` is `@MainActor`.
- New `clients/ios/Brook/Calls/PhoneCalls.swift`, `@MainActor @Observable final class PhoneCalls`:
  - `init(center: CallCenter, system: SystemCalls, client:)`. It holds `[UUID: channelId]` and
    the `UUID` of the live call.
  - `join(channelId:title:handles:)`: does nothing while a call is live or joining, otherwise
    `system.requestStart`.
  - `performStart(id) -> Bool` and `performAnswer(id) -> Bool` both call one private
    `connect(id, channelId) -> Bool`. It returns at once: true for a known id while no other
    call is live, so the action is fulfilled at once and CallKit activates the audio. It runs
    `center.join` in a `Task`. On success it
    calls `reportConnected` and sets `CallModel.onEnded`. On failure it calls
    `reportEnded(.failed)` and leaves `center.joinError` for the screen.
  - `performEnd(id)`: clears the live id at once and runs `center.leave()` in a `Task` kept as
    `leaving`; for a ringing call it runs `incoming.decline()` instead (step 7). `connect`'s
    task awaits `leaving` before `center.join`, so the system's End & Accept leaves the first
    call before joining the second.
  - `performMute(id, muted) -> Bool`:
    - in a listen-only call (`!plan.microphone`), it returns false, so the action is failed on
      purpose: the system's button must not show a state that Brook cannot change;
    - when `muted` already matches the model (`muted == !call.micOn`), it returns true without
      calling `setMic`. This is what stops the correction below from looping;
    - otherwise it returns true and runs `await call.setMic(!muted)`. If the model then still
      differs, the request was refused and `CallModel` rolled back to `confirmed`, so it
      requests `system.requestMute(id, !call.micOn)` once to turn the system screen back.
  - `toggleMute()` (Brook's button): `system.requestMute(id, call.micOn)`.
  - `leave()` (Brook's button): `system.requestEnd(id)`.
  - `audioActivated` and `audioDeactivated` go to `CallAudio`.
  - `sceneChanged(to:)`: on `.background` with the camera on, `call.pauseCamera()`; on
    `.active`, `call.resumeCamera()`. It does nothing on `.inactive`: Control Center or an
    incoming call over Brook does not stop the camera.
  - `endAll()` (sign-out): `center.endAll()` and `reportEnded(.remoteEnded)` for the live call.
- `apple-shared/Brook/Calls/CallModel.swift`:
  - `var onEnded: (@MainActor (FfiEndReason) -> Void)?`, called once when `apply` first sees
    `.ended`. `PhoneCalls` answers a server end with `reportEnded(.remoteEnded)`. On the Mac it
    stays nil.
  - `pauseCamera()`: if the camera is on, `setMedia(video: false)` and remember it.
  - `resumeCamera()`: if paused, `setMedia(video: true)`.
- `clients/ios/Brook/SignedInSession.swift`: `sceneChanged(from:to:)` also calls
  `phone.sceneChanged(to:)`. It is already forwarded from `ChannelListView`'s
  `.onChange(of: scenePhase)`, so there is no second forwarder. The session also owns
  `calls = CallCenter(cameraAtJoin: false)` and
  `phone = PhoneCalls(center: calls, system: system, client:)`, where the init gains
  `system: SystemCalls = CallKitCalls.shared` and the engine factory (for tests). `stop()` also
  starts `phone.endAll()`.
- `clients/ios/Brook/BrookApp.swift` `init`: `CallAudio.prepare()`.
- `clients/ios/project.yml`, `info.properties`:
  `NSMicrophoneUsageDescription: Brook uses the microphone for calls in Brook.`,
  `NSCameraUsageDescription: Brook uses the camera for video in calls in Brook.` (the Mac's
  strings) and `UIBackgroundModes: [audio]`, with a comment: it keeps the app and its socket
  running while a call holds the audio session, and phase 2 adds `voip`. No entitlements file.

**Part b, only if step 0's A or B failed:** a new `clients/ios/Brook/Calls/DirectCalls.swift`,
whole file `#if DEBUG`, implementing `SystemCalls` without CallKit. `requestStart` sets the
`AVAudioSession` category and mode as `CallAudio` does, activates it, and calls
`phone.performStart` and then `phone.audioActivated`. Mute and end call `perform*` directly.
`SignedInSession` picks it only when
`ProcessInfo.processInfo.environment["BROOK_CALLS_WITHOUT_CALLKIT"] == "1"` in a Debug build. The
agent launches it with
`SIMCTL_CHILD_BROOK_CALLS_WITHOUT_CALLKIT=1 xcrun simctl launch booted me.madalin.brook`.

Tests first, new `clients/ios/BrookTests/PhoneCallsTests.swift`, over a `FakeSystemCalls`. The fake
records each request and, like the real provider, answers a request by calling the matching
`perform*`. A `CallCenter` over `FakeMedia`, and a `FakeRealtime` with an open `joinGate` (its `joins` and
`lastHandle` from step 3). One test per Done 12 clause:
1. `testASecondJoinWhileInACallAsksNothing`.
2. `testAnAnswerJoinsThroughTheSamePathAsTheButton`: both end in one `joinCall` with the same
   channel and `publish`.
3. `testTheSystemMuteMovesBrooksButton`, `testBrooksMuteGoesThroughTheSystem`,
   `testARefusedSystemMuteTurnsTheSystemScreenBack` (`FakeRealtime.refusesMedia = true`, which
   makes its `FakeCallHandle` refuse `setMedia` as `FakeHandle(refuse: true)` does: one
   correcting `requestMute(false)`, no second one, `micOn` true) and
   `testAListenOnlyCallFailsTheSystemMute`.
4. `testLeaveEndsTheSystemCall`, `testASystemEndLeavesTheBrookCall`,
   `testAServerEndEndsTheSystemCall`, and in `SignedInSessionTests`,
   `testStopEndsTheBrookAndTheSystemCall` (sign-out).
5. `testLeavingTheScreenWithTheCameraOnAnnouncesItOffAndReturningOn` and
   `testLeavingTheScreenWithTheCameraOffLeavesItOff`, and in `SignedInSessionTests`,
   `testTheBackgroundPausesTheCallsCamera` (through `SignedInSession.sceneChanged`, as the view
   calls it).
6. `testAFailedJoinEndsTheSystemCallAsFailed`.

Mutants, each turning its test red: `toggleMute` sets the model directly (3, the second); no
correcting request after a refusal (3, the third); drop the "already matches" check (3, the
third: the correction loops);
`onEnded` never set (4, the server end); `sceneChanged` resumes the camera on `.inactive` (5);
`SignedInSession.stop` drops `endAll` (4, the sign-out); `SignedInSession.sceneChanged` stops
forwarding to `phone` (5, the session test). The `BrookMediaTests` side, with an
`#if os(iOS)` `CallAudioTests`: `testPrepareKeepsTheAudioOffUntilActivated` and
`testActivationEnablesTheAudio`. Mutant: `prepare` leaves `useManualAudio` false. With part b,
also `testCallKitIsUsedWithoutTheFlag`. The shared `CallModelTests` gain
`CameraAtJoinTests.testOnEndedFiresOnceWhenTheCallEnds` (two `.ended` states, one call).
Mutant: call `onEnded` on every ended state.

`REQUIRED_TESTS` gains the `PhoneCallsTests`, the two new `SignedInSessionTests` tests and
`testOnEndedFiresOnceWhenTheCallEnds`.
`MEDIA_REQUIRED_TESTS` gains the `CallAudioTests`.

Run: `clients/ios/build.sh test`, `clients/macos/build.sh test` (the shared `CallModel` changed;
the Mac's count rises by exactly the one shared test).

Docs: none yet. Nothing is reachable from the screen.

If it stops here: the code is unreachable. The Info.plist declares the usage strings and the
background mode, and no prompt shows, because nothing asks.

### Step 6: the call button, the call screen and the return bar

Subagents: `brook-apple-implementer` (commit `ios: join calls from a conversation`), then
`brook-docs-writer` (commit `docs: calls on iPhone`).

Files:
- `clients/ios/Brook/ConversationView.swift`: a toolbar item in the navigation bar, labelled
  `channels.badge(channel) ?? "Join Call"` with `phone.fill`. It is disabled while not ready,
  while joining, while in a call, or in an archived channel (the Mac's rule, through
  `channels.canJoin`). Tapping it calls `phone.join(...)` with the Mac's `handles` map and
  shows the call screen. A failed join shows `center.joinError` in an alert. `PhoneCalls` gains
  `var showsScreen: Bool`: set on a join or an answer, cleared by hide and by Close.
- New `clients/ios/Brook/Calls/CallScreen.swift`:
  - presented with `.fullScreenCover(isPresented:)` from the root, over the conversation;
  - the `CallStage.split(tiles, pinned:)` stage above a `LazyVGrid` of tiles. Each tile is a
    `UIViewRepresentable` of `RTCMTLVideoView` (aspect fill, a placeholder with the name and a
    `mic.slash` badge when there is no video). The self tile is mirrored
    (`transform = CGAffineTransform(scaleX: -1, y: 1)` on its view), as the system camera
    previews are. What the others receive is not mirrored;
  - names from `tiles(showUsernames: false)`;
  - a bottom bar of system-styled buttons: mute (disabled when `!plan.microphone`, as the Mac),
    camera, the route picker (`AVRoutePickerView` in a representable), hide (`chevron.down`) and
    Leave (red `phone.down.fill`);
  - the banner text from `CallModel.banner`, and `plan.explanation` or `cameraProblem` under it;
  - for an ended call, a "Close" button that calls `center.leave()` and dismisses.
- `clients/ios/Brook/ChannelListView.swift`: a `.safeAreaInset(edge: .top)` on the
  `NavigationStack` while `phone.call != nil && !phone.showsScreen`. It shows the call's title
  and the count, and tapping it presents the screen again. Being on the stack, it shows above
  both the list and the conversation.
- `PhoneCalls.swift`, `#if DEBUG` only: a task started when a call becomes live and cancelled
  when it ends. It is not in the screen, which stops when hidden. Every 2 s it writes a
  `Logger(subsystem: "me.madalin.brook", category: "call")` line
  `inboundAudioEnergy=<sum> framesDecoded=<mid:count,…>` from
  `call.statistics(.subscribe, type: "inbound-rtp")`. Numbers only, `privacy: .public`. This is
  how the simulator check reads the phone's side (spec §7). For this, the shared `CallMedia`
  protocol gains `func statistics(_ pc: FfiPcKind, type: String) async -> [[String: String]]`
  (`WebRTCEngine` already has it), the shared `FakeMedia` answers `[]`, and `CallModel` gains a
  plain passthrough `statistics(_:type:)`.

Tests first: the view is checked in the simulator, not by unit tests (as in the conversation
plan). The button's rule is a function, `PhoneCalls.canJoin(_ channel:)`, so it can be tested:
`PhoneCallsTests/testTheJoinButtonIsOffWhileInACall` (false while a call is live or joining) and
`testHidingKeepsTheCallAndShowsTheBar` (`showsScreen` false, the call still live). Mutants: drop
the in-call clause; make hide call `leave`. Each must turn its test red. Both are added to
`REQUIRED_TESTS`.

Simulator check (section 3a): Done 1, 2, 3 (Brook's button), 5, 6 (Leave), 10 (microphone), 11.
Each result goes in the pull request, with the log lines and screenshots.

Docs (commit 2):
- `clients/ios/README.md`: "What the app does" gains calls; "Known limits" gains no ring yet
  (until step 7), no ring in the background (phase 2), no sharing from the iPhone, the simulator
  has no camera; "Later work" changes its Calls and CallKit lines and keeps Push.
- `docs/user-guide.md`: line 158 becomes "macOS, GNOME and iPhone", plus a short iPhone
  paragraph: the call button, the camera off at join, the system call screen, hide and return.
- `docs/FEATURES.md` §7: iOS joins calls.
- `docs/PROTOCOL.md`: no change. The reviewer confirms that §3a still describes no push for this
  phase.

If it stops here: calls work on the iPhone except the ring (Done 1 to 8, 10 and 11). The
README says so. This is a shippable feature.

### Step 7: the DM ring while open (after #293)

Subagent: `brook-apple-implementer`. Commits: `apple: share the ring rule` (a move), then
`ios: ring for a DM call with CallKit`.

**If #293 has not merged when steps 1 to 6 are done**, this step waits: the ring rule is #293's
(spec §8, question 6). The main agent records on #355 that Done 9 is open. #355 stays open, and
step 8 runs without Done 9. #293 has to rebase on step 2 anyway, and by hand (risk 6). That rebase belongs to
#293, not to this plan.

Files:
- Commit 1: `IncomingCalls` and `IncomingCallsTests` move to `clients/apple-shared` with `git mv`.
  `SystemRinger` (AppKit, `NSSound`) is cut into a Mac-only `Brook/Calls/SystemRinger.swift`
  first. `Ringer.start()` and `stop()` become `start(_ r: IncomingCalls.Ringing)` and
  `stop(_ r: IncomingCalls.Ringing, why: IncomingCalls.RingEnd)`; the Mac's ringer ignores both.
  `enum RingEnd { case answered, declined, answeredElsewhere, callerLeft, timedOut, sessionEnded }`.
  `end(finishing:)` takes it from its caller:
  - `answer()` gives `.answered` and `decline()` gives `.declined`;
  - the timeout gives `.timedOut`, and `stop()` gives `.sessionEnded`;
  - in `reevaluate`, a call now seen with 2 or more people gives `.answeredElsewhere`, and one
    whose announcement is gone (no call id, a count of 0, or another call id) gives
    `.callerLeft`;
  - anything else that stops the rule (this device joining it, the channel no longer a DM)
    gives `.answered` when `localChannel` is that channel, and `.callerLeft` otherwise.
  The shared `IncomingCallsTests` gain one test per reason, such as
  `testAnAnswerElsewhereEndsTheRingAsAnsweredElsewhere`. `alert` stays an
  optional closure the Mac sets. The Mac's tests prove no change, and `REQUIRED_TESTS` gains
  every moved test.
- Commit 2:
  - In #293, `ChannelsModel` (already shared) owns `incoming`, and its ringer defaults to the
    AppKit-only `SystemRinger`. Here, `ChannelsModel` takes the ringer with no default in
    shared code: the Mac passes `SystemRinger()`, and iOS (`SignedInSession`) passes a
    `PhoneRinger`. `PhoneCalls` uses `channels.incoming`, never a second `IncomingCalls`.
    `ChannelsModel.stop()` already calls `incoming.stop()`, so a sign-out ends the CallKit ring
    with `.sessionEnded`.
  - It also gains `ringIds: [String: UUID]` (call id → system call), never reused, so the same
    call is never reported twice.
  - The new `PhoneRinger: Ringer`: `start(r)` reports the call (`reportIncoming`, caller = the
    DM's `channels.title`) unless `ringIds[r.callId]` already exists. `stop(r, why:)` reports
    `.remoteEnded` for `.callerLeft` and `.sessionEnded`, `.answeredElsewhere` for
    `.answeredElsewhere`, and `.unanswered` for `.timedOut`. It reports nothing for `.answered`
    and `.declined`, which the system started itself.
  - `performAnswer` on a ringing id: `incoming.answer()`, then `connect` (the camera is off, as
    the button's join). `performEnd` on a ringing id: `incoming.decline()`.

Tests first, in `PhoneCallsTests.swift`:
- `testTheSameCallIdIsReportedOnce` (two `observe` calls for one call id, an end, and an
  `observe` again);
- `testAnsweringJoinsWithTheCameraOff`;
- `testDeclineEndsOnlyThisPhonesRing`;
- `testTheRingEndsWhenTheCallerHangsUp` (`.remoteEnded`), `testTheRingEndsWhenAnsweredElsewhere`
  (`.answeredElsewhere`) and `testTheRingEndsAfterTheLimit` (`.unanswered`, with a short `limit`).
  Each asserts the reason given to `reportEnded`;
- `testAChannelCallNeverRings`;
- `testAnsweringASecondCallLeavesTheFirstFirst` (the system's End & Accept: `performEnd` on the
  live call, then `performAnswer`, gives one `leave` before one `joinCall`).

Mutants: drop the `ringIds` check (the first test fails); map every reason to `.unanswered` (the
hang-up and elsewhere tests fail). All the tests, and the new `IncomingCallsTests`, are added to
`REQUIRED_TESTS`.

Simulator check: Done 9, only if step 0's C passed (section 3a).

Docs: `clients/ios/README.md` (the ring while open) and `docs/user-guide.md` (the iPhone rings
for a DM call while Brook is open).

If it stops here: everything in phase 1 is built.

### Step 8: the owner's check on a real iPhone

Not an agent step. It runs on an iPhone (iOS 26) against the live server, in a test channel and
a test DM, with the Mac app as the other side. Brook is launched from the Home Screen, not from
Xcode. The items are the spec's [device] items: 2 (by ear, 2 minutes on the speaker, no echo
reported by the Mac side), 3 (the system screen), 4, 6 (the system screen and the dots), 7, 8,
9, 10 (the camera), and whatever step 0 moved to the device. Also: the self-view is mirrored
while the Mac sees the phone's camera unmirrored, and a refused or listen-only mute leaves the
system screen and Brook's button in agreement. The results go into a comment on
#355, and into the spec's "as built" notes through `brook-spec-writer`.

## 3. Migrations and production

No database, route or protocol change. On the server, only the dev harness page changes, and
it is served only with `BROOK_DEV_HARNESS=true`. The iOS app keeps no new data. The Mac's
behavior does not change. To roll back, revert the pull requests newest first. Each step leaves
a working app, so a partial rollback works too.

### 3a. The simulator checks (steps 6 and 7)

Setup, as in the iOS README: from `deploy/`, `make up` and `make media`, with
`BROOK_DEV_HARNESS=true` in the local `.env` only. The app is installed and launched with
`clients/ios/build.sh run`. Accounts A (on the phone) and B (the harness, standing in for the
Mac) are throwaway local accounts, made through the API as in the conversation plan's §3a. Their
passwords are generated into a mode-600 file in the scratchpad, never printed and never put on a
command line. The harness is driven by a Playwright script kept in the scratchpad: Chrome with
`--use-fake-ui-for-media-stream --use-fake-device-for-media-stream`, and
`?tone=1&autojoin=1` with B's handle and the channel in the URL. The phone's side is read with
`xcrun simctl spawn booted log stream --predicate 'subsystem == "me.madalin.brook" AND category == "call"'`.
No Mac app is run at all: it shares the owner's defaults and Keychain.

- **Done 1:** tap the button. Screenshots of the button with its count, the microphone prompt
  and the call screen.
- **Done 2:** the phone's `inboundAudioEnergy` rises while the harness sends its tone. After
  `setMedia(false, true)` on the harness, it rises by less than 1% of before. In the other
  direction, `afplay` plays in a loop on the Mac, and the harness's `stats().inboundAudioEnergy`
  rises.
- **Done 3:** Brook's mute flattens the harness's energy, and the harness's roster shows
  `audio: false` for A.
- **Done 5:** `framesDecoded` rises for the harness's camera mid, and for its fake screen after
  `shareScreen(true)`. A screenshot shows the screen on the stage.
- **Done 6:** Leave. The harness's `participants` drops A within 5 s.
- **Done 10:** `xcrun simctl privacy booted revoke microphone me.madalin.brook`, then join. The
  listen-only text shows, the mute button is disabled, and the harness's roster shows A with no
  audio.
- **Done 11:** in a call, `docker compose stop caddy`, wait 10 s, `docker compose start caddy`.
  The screen shows "Reconnecting…" and then clears. The harness lost its own socket too, so it
  joins again, and the phone's energy rises again. Then the ended reasons: B removes A from the
  channel, and the text is the iPhone wording.
- **Done 9 (step 7, only if step 0's C passed):** B sends `call.join` for the DM over its own
  WebSocket. The app's log shows a report to CallKit, and a screenshot shows the incoming
  screen if the simulator draws it. Then each end case: B leaves, a second session of A answers
  over the WebSocket, and 45 s pass. Each must end the report. A channel call must give no
  report.

## 4. Risks

1. **The camera can be announced on for a moment at join. This is accepted.** The server's
   intent starts on (`calls.py`, `video_intent = True`), and the phone's `call.media` off can
   land just after its publish. Core sends it as soon as `join_call` returns. The server applies
   it at once (`calls.py`, `_media` and `refresh_media`), so the others see at most one
   camera-on `call.participant` event. No frame is ever sent, because the engine is not armed.
   No timing check is made: an event order seen in one run proves nothing about the next. The
   alternative that removes the window, an initial intent on core's `join_call`, was rejected
   (section 1).
2. **WebRTC's audio on iOS and the CallKit hand-over.** A wrong order (audio enabled before
   `didActivate`, or `isAudioEnabled` never reset) works in the simulator and fails on a locked
   phone, or after a phone call. `CallAudioTests` cover the order, and Done 7 and 8 on the device
   are the real proof. If audio is lost after a phone call ends the Brook call, the first thing
   to check is that `didDeactivate` reached `CallAudio.deactivated`.
3. **The app's size.** Linking `BrookMedia` embeds `WebRTC.framework` (arm64), tens of MB. Step
   2 records the `.app` size before and after. Nothing is thinned on iOS: the framework's iOS
   slice is already per platform.
4. **`REQUIRED_TESTS` grows by hand.** About 60 names move in or are added, plus a second array
   for the engine. A test left out still runs, but its silent removal would go unnoticed. Each
   step's reviewer checks the count against `grep -c 'func test'` in the files it touched.
5. **The engine tests on the simulator add time** to every `build.sh test`: a second
   `xcodebuild` and, the first time, the WebRTC download. Accepted, because nothing else runs
   them on iOS (section 5, decision 1).
6. **#293 and step 2 edit the same lines.** #293 rewrites the parts of `CallCenter.swift` that
   step 2's commit 1 changes: the clearing of `quit.leaveActiveCall`, and `leave()`'s clearing
   and its `channelId`. So whichever lands second rebases by hand, not mechanically. Each of
   #293's edits is redone on the shared file against `leaveForQuit` instead of
   `quit.leaveActiveCall`, and the Mac's tests (the quit tests included) must pass after it.
7. **CallKit in the simulator may differ from the device** even if the spike passes, for
   example in when `didActivate` fires. The device check repeats Done 1 to 3, 6 and 9.
8. **Mac echo is not in this plan, but iOS echo is new.** Voice Processing I/O is on by default
   on iOS. If step 8 reports echo, it goes back to the owner, as the Mac's fallback did.

## 5. Decisions for the owner

1. **The engine tests run inside `clients/ios/build.sh test`** (the spec allows it, Done 12).
   Recommended: yes, because otherwise nothing runs them on iOS. The cost is a few minutes per
   run.
2. **If #293 is not merged when step 7 is reached**, step 7 waits and #355 stays open with Done 9
   unproven (step 7). The alternative is that this plan builds the ring rule in shared code and
   #293 rebases onto it. That reverses §8 question 6, so it is the owner's call.
