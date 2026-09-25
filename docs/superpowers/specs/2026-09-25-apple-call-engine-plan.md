# Apple call engine + macOS call UI (C2/C3) — implementation plan

> Status: **approved** (round 1 applied; round 2 skipped — see log) · 2026-09-25
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
  **with its field values** (distinct sentinels for candidate `candidate`/`sdpMid`/`sdpMLineIndex`,
  stream mid → participant, ice server fields; mutation: swap two fields / drop `set_ice_servers`
  forwarding → fails); foreign errors map to `EngineError` and back; unknown kind/source → `Unknown`.
  A join through the in-process test origin returns a handle; the publish offer produced by the fake
  engine reaches the server (not merely `Connected`, which core sets before publishing).
- **`FfiCallHandle` drop → leave:** dropping the last Swift reference ends the call (`call.leave` seen by
  the server, engine `close()` called). Mutation: engine holds the handle strongly → fails.
- **Swift→Rust→Swift round trip** (E0's package, clean build): a Swift fake engine joined through a
  local scripted server — `create_publish_offer` is awaited by core and its SDP reaches the server.

### E2 — `BrookMedia` target and `WebRTCEngine` core
- `Package.swift`: `WebRTC` binary target (stasel 153.0.0 URL + checksum), `BrookMedia` target +
  product. Engine: serial queue, fence-first `close()` + `closed` completion, weak `attach(handle)`
  with ordered buffer drain, explicit transceiver directions, subscribe PC reuse + mid ownership.
- **Test plumbing (added first):** an injectable `CandidateSink` so a test can route engine A's
  publish candidates into engine B's subscribe PC and back without a server; a synthetic video
  source (`RTCVideoSource` fed `RTCVideoFrame`s from a CVPixelBuffer on a timer) and a synthetic
  audio source (a tone through a custom `RTCAudioDevice` that reads from a buffer) so no camera,
  microphone, TCC prompt or server is involved.
- **Loopback test**: A-publish ↔ B-subscribe offer/answer + trickle both ways → selected candidate pair,
  decoded frame counter rising, inbound audio bytes rising.
- **Check:** loopback green; fence (each op held, `close()` → op errors, `closed` completes, capture
  state stopped — a start that completes after the fence is stopped); capture start bounded at 5 s
  (a capture stand-in that never starts → op errors in ≤ 5 s); stalled queue → sync methods still
  return; attachment race incl. end-of-candidates; handle dropped → engine released (weak-reference
  mutation fails it); mid reuse/removal; mutations on each.

### E3 — Capture and permissions
- `RTCCameraVideoCapturer` (≤ 720p30), default-ADM audio; camera-off = stop capturer (restart on);
  mute = `isEnabled`; absent track: only *enable* is rejected. Permission flow resolved before join,
  microphone first; denial/restricted/cancel → listen-only / audio-only.
- **Check:** unit tests with an injected capture stand-in for the permission branches, camera-off
  (capture session state, not `isEnabled`) and camera back on **without renegotiation** (no new offer).
  UI texts required by the design are tasked here: the degradation explanations, the rejoin-to-publish
  guidance, the mute tooltip. Real TCC behaviour is verified in E6.

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
- **Realtime startup:** after sign-in the app subscribes to events **before** `start_realtime`, and Join
  is enabled only once the socket is ready (a `Ready` event), so the first join never hits
  `Disconnected`.
- **Check:** view-model unit tests (badges from events, Join disabled until ready, controls disabled
  when a track is absent, quit handshake replies exactly once incl. timeout — mutation: reply twice /
  never); off-screen renders of the call window states.

### E6 — Live acceptance
- `itest.sh` requires the new call suite by name **and keeps the login suite gate**. Call suite (test
  server, account `mac`, channel `calltest`): join → Connected; publish answer applied; a second
  participant's subscribe offer answered; leave → Ended(Left) and `closed`.
- **Second participant procedure:** the Linux side starts `call_participant` as `linux` in `calltest`,
  confirms it is joined and receiving nothing yet, then the Mac joins; the Linux side reports frames,
  audio bytes and the roster every 5 s and leaves on request; the server session reads Janus logs by
  call_id + UTC time if media does not flow.
- **Real TCC (Finder-launched, signed, sandboxed):** fresh prompts on first join; microphone denied →
  no camera prompt, listen-only with the explanation; camera denied → audio-only, and microphone mute
  still works; camera indicator off after camera-off and after leave.
- **Human/peer acceptance:** Finder-launched Brook.app in `calltest` with the Linux participant: two-way
  video + audio, mute/camera shown on the other side, leave cleans both rosters. **Echo check:** frame
  and byte counters cannot show echo, so a person listens — the owner on the Linux side (GNOME client
  with a headset) speaks while the Mac plays through laptop speakers for 2 minutes and reports whether
  they hear themselves; or the Linux side records its received audio while playing a known clip, and
  the recording is checked for that clip. CPU noted.
- **Check:** all of the above recorded in the PR; the merge is gated on the echo check.

### Deviations found while implementing
- **E2, audio device header.** The macOS slice of WebRTC M153 compiles `RTCAudioDevice` and
  `ObjCAudioDeviceModule` (checked in the binary) but ships the header only in its iOS slices. A
  declarations-only C target, `WebRTCAudioDevice`, re-exports the upstream header unchanged for
  macOS (BSD licence kept alongside). With it the synthetic audio device of E2 works as planned, and
  the E6 fallback (a custom device with Apple voice processing) is possible on macOS.
- **E2, remote track wrappers.** `receiver.track` returns a new ObjC wrapper on each call, and a
  wrapper's dealloc removes every renderer added through it. The engine keeps one wrapper per owned
  mid; the loopback test's frame counter failed until it did (0 frames rendered while 241 decoded).

- **E4, signing.** The Finder-launch gate failed on the first Release bundle: under the hardened
  runtime, library validation refuses the embedded `WebRTC.framework` when neither the app nor the
  framework carries a Team ID (ad-hoc signing). Release is now signed with the owner's Developer ID
  identity (owner's choice), kept in a gitignored `clients/macos/Local.xcconfig` via an optional
  include in `Signing.xcconfig`; `build.sh release` refuses to run without it. No
  `disable-library-validation`.

- **After review (Linux client), camera failure.** The plan had a camera that fails to start
  fail the operation; core answers an offer error by ending the call, so a busy or missing camera
  dropped a working audio call. A camera problem is now reported to the UI (camera shown off, with
  the reason) and the call goes on; only the close fence still aborts the offer.

## Where this fails

| Point | Failure | Response |
|---|---|---|
| E0 | `@_exported import` / `@retroactive` behave differently than expected across the split | E0 runs first, alone; if it fails, fall back to one target in Swift 5 mode for generated + a separate Swift 6 target that wraps (no re-export) |
| E1 | UniFFI async foreign trait + records combination hits a generator bug not seen in the spike | reduce to the spike's shape (String/primitive args) and convert in Swift |
| E2 | synthetic video source not available in the ObjC API | use `RTCVideoSource` + a timer feeding `RTCVideoFrame`s from a CVPixelBuffer |
| E2 | loopback flaky on CI-less local runs | frame/byte counters with generous bounds; loop 20× |
| E4 | re-signing order breaks the app signature | verify on the Release bundle every time; the check is the gate |
| E6 | UDP from the Mac to 192.168.1.192 or the Linux side's subnet blocked | the Linux side confirms routing on its first join; the server session reads Janus logs by call_id |
| E6 | AEC3 echo on speakers | the named fallback (custom `RTCAudioDevice` with Apple voice processing) becomes its own spec before merge |

## If it stops halfway
E0 alone is a safe refactor (tests prove behaviour unchanged). E1 adds FFI API nothing calls yet. E2–E3
add a package target the app doesn't link until E4. **E4 and E5 change the shipped binary, widen the
sandbox and expose capture: the branch is not mergeable until E6's gates pass** — an intermediate state
that links the engine is not evidence it is safe to ship. The branch is stacked on #11 and merges only
after #7 and #11. Tasks run strictly in order E0 → E6.

## Review log
**Round 1 — Codex + Vibe (Heavy).** All accepted, all additive: injectable candidate sink + synthetic
audio and video for a server-less, TCC-less loopback; capture bound, late-start stop, restart without
renegotiation, handle-drop → leave and weak-reference release tests; real TCC denial checks in E6 with
the design's UI texts tasked; subscribe-before-start and Join gated on readiness; FFI field-value,
error and `Unknown` assertions; a human (or recorded-clip) echo check with a concrete second-participant
procedure; login gate kept in `itest.sh`; E4–E5 not mergeable before E6; strict task order. Round 2
skipped: nothing disputed, every change adds a check.
