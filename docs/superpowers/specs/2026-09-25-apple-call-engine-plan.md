# Apple call engine + macOS call UI (C2/C3) — implementation plan

> Status: **draft** — awaiting review · 2026-09-25
> Implements: [2026-09-24-apple-call-engine-design.md](2026-09-24-apple-call-engine-design.md) (approved, Heavy)
> Branch: `feat/apple-call-engine`, stacked on #11 (→ #7). Heavy dial: both reviewers at this gate.

Each task ends in a check; tests are written first and seen failing under a named mutation.

### E0 — Package split (proves the Swift 5 / Swift 6 arrangement first)
- `build-xcframework.sh`: generate into `Sources/BrookCoreGenerated/` (old `Generated/` removed; the
  `.gitignore` entries follow). `Package.swift`: `BrookCoreGenerated` (`.swiftLanguageMode(.v5)`) ←
  `BrookCore` (Swift 6, `@_exported import BrookCoreGenerated`; redaction + observer move here with
  `@retroactive` conformances).
- **Check:** from a clean tree (`rm -rf` build outputs + `.build`): script → `swift build` → all
  existing BrookCore tests pass (redaction mutation still caught across the module boundary); the
  macOS app builds and its 25 tests pass.

### E1 — Bindings: call API over FFI (Rust)
- `MediaEngine` as `#[uniffi::export(with_foreign)] #[async_trait]` FFI trait with FFI records;
  `EngineAdapter(Arc<dyn FfiMediaEngine>)` implements `brook_core::MediaEngine`, forwarding **every**
  method (incl. `set_ice_servers`); FFI errors ↔ `EngineError`.
- `FfiBrookClient`: `start_realtime`, `list_channels`, `subscribe_events(ServerEventListener)`
  (`ChannelCall`, `Ready`), `join_call(channel_id, engine, publish) -> FfiCallHandle`.
- `FfiCallHandle`: `subscribe_state(CallStateListener)` (latest-state, cancellable, like auth),
  `local_candidate`, `engine_failed`, `set_media`, `republish`, `leave` — async ones via the owned runtime.
- **Check:** Rust tests with a fake foreign engine: every adapter method reaches the foreign object
  (mutation: drop `set_ice_servers` forwarding → fails); a join through the in-process test origin
  (reusing core's harness pattern) returns a handle whose state reaches `Connected`.
- **Swift→Rust→Swift round trip** (E0's package, clean build): a Swift fake engine joined through a
  local scripted server — `create_publish_offer` is awaited by core and its SDP reaches the server.

### E2 — `BrookMedia` target and `WebRTCEngine` core
- `Package.swift`: `WebRTC` binary target (stasel 153.0.0 URL + checksum), `BrookMedia` target +
  product. Engine: serial queue, fence-first `close()` + `closed` completion, weak `attach(handle)`
  with ordered buffer drain, explicit transceiver directions, subscribe PC reuse + mid ownership.
- **Loopback test** (two engines, no server, synthetic video source instead of the camera so it runs
  headless): offer/answer + trickle both ways → selected candidate pair, decoded frame counter rising,
  inbound audio bytes rising.
- **Check:** loopback green; fence (each op held, `close()` → op errors, `closed` completes); stalled
  queue → sync methods still return; attachment race incl. end-of-candidates; mid reuse/removal;
  mutations on each.

### E3 — Capture and permissions
- `RTCCameraVideoCapturer` (≤ 720p30), default-ADM audio; camera-off = stop capturer (restart on);
  mute = `isEnabled`; absent track: only *enable* is rejected. Permission flow resolved before join,
  microphone first; denial/restricted/cancel → listen-only / audio-only.
- **Check:** unit tests with an injected capture stand-in for permission branches and camera-off
  (capture session state, not `isEnabled`); real-camera behaviour is verified in E6.

### E4 — macOS packaging
- `project.yml`: link `BrookMedia`; entitlements add `network.server`, `device.camera`,
  `device.audio-input`; Info.plist usage strings; post-embed phase thins the **embedded** WebRTC copy
  to arm64 and re-signs it before the app is signed.
- **Check (Release bundle):** `lipo -archs` = arm64 for the app and the embedded framework;
  `codesign -v --deep --strict` passes; `codesign -d --entitlements -` shows exactly the five keys;
  the app launches from Finder.

### E5 — Call UI
- Sidebar channel list with `ChannelCall` badges; Join; call window (tiles via `RTCMTLNSVideoView`,
  roster labels, self-view, mute/camera/leave + shortcuts, Reconnecting/Ended banners); window close
  and app quit (`.terminateLater` handshake) await `leave()` + `closed`, bounded.
- **Check:** view-model unit tests (badges from events, controls disabled when a track is absent,
  quit handshake replies exactly once incl. timeout — mutation: reply twice / never); off-screen
  renders of the call window states.

### E6 — Live acceptance
- `itest.sh` requires the new call suite by name. Call suite (test server, account `mac`, channel
  `calltest`): join → Connected; publish answer applied; a second participant's subscribe offer
  answered; leave → Ended(Left) and `closed`.
- **Human/peer acceptance:** Finder-launched Brook.app in `calltest` with brook-linux's headless `linux`
  participant (it reports frames/audio received from the Mac) and the owner on screen: two-way video +
  audio, mute/camera shown on the other side, leave cleans both rosters, camera indicator off after
  camera-off and after leave. **Echo check on laptop speakers, 2 minutes.** CPU noted.
- **Check:** all of the above recorded in the PR; the merge is gated on the echo check.

## Where this fails

| Point | Failure | Response |
|---|---|---|
| E0 | `@_exported import` / `@retroactive` behave differently than expected across the split | E0 runs first, alone; if it fails, fall back to one target in Swift 5 mode for generated + a separate Swift 6 target that wraps (no re-export) |
| E1 | UniFFI async foreign trait + records combination hits a generator bug not seen in the spike | reduce to the spike's shape (String/primitive args) and convert in Swift |
| E2 | synthetic video source not available in the ObjC API | use `RTCVideoSource` + a timer feeding `RTCVideoFrame`s from a CVPixelBuffer |
| E2 | loopback flaky on CI-less local runs | frame/byte counters with generous bounds; loop 20× |
| E4 | re-signing order breaks the app signature | verify on the Release bundle every time; the check is the gate |
| E6 | UDP from the Mac to 192.168.1.192 or brook-linux's subnet blocked | brook-linux confirms routing on its first join; the server session reads Janus logs by call_id |
| E6 | AEC3 echo on speakers | the named fallback (custom `RTCAudioDevice` with Apple voice processing) becomes its own spec before merge |

## If it stops halfway
E0 alone is a safe refactor (tests prove behaviour unchanged). E1 adds FFI API nothing calls yet. E2–E3
add a package target the app doesn't link until E4. E4 changes the app's entitlements only when it also
links the engine. The branch is stacked on #11 and merges only after #7 and #11.
