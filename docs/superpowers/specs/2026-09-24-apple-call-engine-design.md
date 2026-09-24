# Apple call engine + macOS call UI (C2/C3)

> Status: **revised after Heavy review round 1** (Codex + Vibe) · 2026-09-25
> Review dial: **Heavy** — camera/microphone access, a wider sandbox (incoming network), a third-party
> binary in the app. Both external models, given evidence.
> Builds on: core call signaling (PR #13, live-proven on GStreamer), Apple FFI bridge (#7), macOS app (#11).
> Evidence from spikes (2026-09-24): see §2.

## 1. Goal

A macOS user signs in, sees their channels, sees "call in progress · N" on a channel, joins, and
has a two-way audio+video call with a GTK participant (or the server's browser harness) through the
shared test server; can mute, turn the camera off, and leave. iOS reuses the engine later.

## 2. Facts established by spikes (not assumptions)

| Question | Result |
|---|---|
| Can Swift implement an async Rust trait (UniFFI 0.32 `with_foreign`) that core awaits from its own Tokio runtime? | **Yes** — ran end to end. Sync methods are called on Rust worker threads, never the main thread. |
| Does the generated Swift compile in Swift 6 mode? | **No** for async foreign traits (`SendingClosureRisksDataRace`). **Yes** in Swift 5 mode. |
| libwebrtc artifact | stasel/WebRTC **M153**, sha256 `3e3a8946…2b78f` = upstream `Package.swift`; built by public GitHub Actions from chromium.googlesource. Owner approved after a trust check. |
| Software H.264 in the binary? | **None** (0 OpenH264/FFmpeg/x264 symbols); `RTCVideoEncoderH264` → VideoToolbox. |
| macOS slice | universal (x86_64+arm64), dynamic framework. |
| ICE under App Sandbox with `network.client` only | **only TCP-active port-9 candidates, no UDP** → no media to ICE-lite Janus. |
| … plus `network.server` | UDP host candidates gathered. |

## 3. Design

### 3.1 Bindings (`bindings/apple`, Rust) — expose the call API
- `FfiBrookClient`: `start_realtime()`, `list_channels() -> Vec<FfiChannel>`, `subscribe_events(listener)`
  (a `ServerEventListener` foreign trait receiving `ChannelCall` and `Ready`; other events ignored
  for now), `join_call(channel_id, engine, publish) -> Arc<FfiCallHandle>`.
- `MediaEngine` exported as `#[uniffi::export(with_foreign)] #[async_trait]` with the exact core
  signatures; an adapter struct wraps the foreign object as `Arc<dyn brook_core::MediaEngine>`.
- `FfiCallHandle`: `subscribe_state(listener)` (same latest-state listener pattern as auth),
  `local_candidate`, `engine_failed`, `set_media`, `republish`, `leave` (all async ones hop onto the
  owned runtime, like `login`).
- New wire/FFI records: `FfiCallState`, `FfiParticipant`, `FfiSubStream`, `FfiIceCandidate`,
  `FfiIceServer`, `FfiChannel`; enums with `Unknown` fallbacks.

### 3.2 Swift package layout
`bindings/apple/swift/BrookCore/Package.swift` gains (and `build-xcframework.sh` generates into
`Sources/BrookCoreGenerated/` instead of `Sources/BrookCore/Generated/`, cleaning the old path):
- target **`BrookCoreGenerated`** — the UniFFI Swift, **Swift 5 language mode** (spike: it does not
  compile in 6); depends on the `BrookCoreFFI` binary target.
- target **`BrookCore`** — hand-written, Swift 6, depends on `BrookCoreGenerated` and re-exports it
  (`@_exported import`); the token-redaction extensions move with it and are declared
  `@retroactive` (conformances on another module's types).
- target **`BrookMedia`** — the libwebrtc engine, Swift 6, depends on `BrookCore` and
  `.binaryTarget(name: "WebRTC", url: <stasel 153.0.0 zip>, checksum: "3e3a8946…2b78f")`;
  exported as a **`BrookMedia` product**, linked by the app in `project.yml`.
- The macOS app embeds `WebRTC.framework` once. A post-embed build phase **thins the embedded copy**
  (never SwiftPM's downloaded artifact) to arm64 and **re-signs that nested framework** with the app's
  identity before the app itself is signed. Verified on the Release bundle: `lipo -archs` = arm64,
  `codesign -v --deep --strict` passes, the app launches. No library-validation exception.

### 3.3 `BrookMedia.WebRTCEngine` (implements `MediaEngine`)
- **Engine-owned serial queue.** Every WebRTC call runs on one engine `DispatchQueue`. The sync
  trait methods (`add_remote_candidate`, `set_local_media`, `set_ice_servers`) only **enqueue** and
  return — WebRTC setters can block on WebRTC's own threads, and core calls these inline in its call
  task. Async methods enqueue and await the result. No lock is held across any WebRTC call or any
  call into Rust. A failure of deferred work is reported through `engine_failed`.
- One `RTCPeerConnectionFactory` (default encoder/decoder factories → H.264 via VideoToolbox, VP8
  fallback, Opus). Unified Plan; **every transceiver's direction set explicitly** (the default is
  sendrecv).
- **Publish PC** (sendonly): audio track from the default audio device module (WebRTC AEC3 on
  macOS — measured in §6), video from `RTCCameraVideoCapturer` at ≤ 720p30. `create_publish_offer` =
  create offer + set local description; capture start is bounded (≤ 5 s).
- **Subscribe PC** (recvonly), created lazily on the first `apply_subscribe_offer` (full
  remote-description → answer → local-description transition); kept across re-offers; transceivers
  reused, and mid ownership updated from the **latest** `streams` even when no new track callback fires.
- **Handle attachment:** candidates and fatal errors produced before the handle exists are buffered;
  `attach(handle)` drains them atomically, in order (including end-of-candidates), then forwards
  live. The engine holds the handle **weakly** — a strong reference would keep the call alive after
  the UI dropped it (the call task retains the engine), defeating core's drop-to-leave.
- **Camera off** stops `RTCCameraVideoCapturer` (the camera and its privacy indicator turn off) and
  keeps the track/transceiver, so turning it back on restarts capture without renegotiation.
  **Mute** sets the audio track's `isEnabled = false`: silence is sent, the microphone stays open
  (its indicator stays on) — the same as other call apps, stated in the UI tooltip. If a track does not
  exist (permission denied), `set_local_media` reports that through `engine_failed`-free return:
  the enqueued work returns an error the next async op surfaces, and the UI disables the control.
- **`close()` and the fence:** the fence flag is set **first**, then capture is stopped (awaited), both
  PCs are closed, and late UI updates are discarded. Every operation checks the fence at entry, after
  each await, and before installing any resource; a capture that finishes starting after the fence is
  stopped immediately and the op errors. The engine publishes an observable **`closed`** completion.
- **Every end path closes the engine:** core's single `finish()` calls `close()` for local leave,
  handle drop, engine failure, remote `call.ended`, `Expired`, session change. The app adds the one
  path core cannot see: **app quit** (`applicationShouldTerminate` → `leave()` and await `closed`,
  bounded).
- Threading: WebRTC delegate callbacks hop onto the engine queue; UI state is published to the main
  actor; the Rust worker threads that call the sync methods never wait on either.

### 3.4 Permissions and entitlements (macOS)
- Entitlements: `app-sandbox`, `network.client`, **`network.server`** (UDP ICE — §2),
  `device.camera`, `device.audio-input`. Nothing else. **What `network.server` opens:** the process
  (including the WebRTC framework) may accept incoming TCP connections and receive UDP on any port it
  binds; it does not itself create listeners or bypass the firewall or TCC. Brook binds only the
  ICE sockets libwebrtc creates for a call, and they are closed on every end path (§3.3).
- Info.plist: `NSCameraUsageDescription`, `NSMicrophoneUsageDescription` ("…for calls in Brook").
- Permission is resolved **before** `join_call(…, publish:)` — the prompt's human time never eats
  into the engine's capture budget — and only when the user joins, never at launch. The microphone
  is asked first: denied/restricted/cancelled → join listen-only (`publish = false`), camera not asked,
  and say so. Microphone granted, camera denied → join audio-only (no video track) and say so.
  Unavailable controls are disabled: core cannot promote a listen-only call to publishing
  (`republish` needs an established publish PC), so a listen-only user rejoins to publish.

### 3.5 macOS UI (C3, minimal)
- After sign-in: a `NavigationSplitView` sidebar of channels (`list_channels`), each showing
  "● Call · N" from `ChannelCall` events; a **Join call** button.
- Call window: a grid of video tiles (`RTCMTLNSVideoView` wrapped in `NSViewRepresentable`) labelled
  from the roster; self-view; controls: mute, camera, leave (⌘⇧M, ⌘⇧V, ⌘W); status banner for
  Reconnecting / Ended(reason). Closing the window (or quitting) awaits `leave()` **and** the
  engine's `closed` (bounded) before it closes, so capture has provably stopped.

## 4. Not doing
- iOS app, CallKit, push (later; the engine is written to be shared).
- Screen share, simulcast, TURN, Apple voice-processing AEC (possible custom ADM later — §6 decides).
- Chat UI (messages) — only the channel list needed to join a call.

## 5. Tests
- Rust (bindings): the call API round-trips through the FFI types; the adapter forwards **every**
  trait method (incl. `set_ice_servers`) and maps engine errors.
- **Swift→Rust→Swift round trip** in a clean build: a Swift `MediaEngine` awaited by core through the
  split package (proves the mixed-language-mode package links and runs).
- Swift unit, `WebRTCEngine` **loopback** (no server): engine A's publish offer applied as engine B's
  subscribe offer, the answer and trickled candidates exchanged **both ways**; assert a selected
  candidate pair, **decoded video frames** (frame counter rising) and **received audio** (inbound
  RTP audio bytes/levels rising) — not merely `connected` or a track object.
- Fence: each description op held at a gate while `close()` runs → the op errors, capture stays
  stopped (capture-session state checked, not `isEnabled`), and `closed` completes.
- Camera off → capture session not running; back on → running, no renegotiation.
- Sync methods return while the engine queue is deliberately stalled (core keeps signaling).
- Handle attachment racing buffered candidates incl. end-of-candidates → delivered once, in order.
- Mid reuse and removal across re-offers updates tile ownership.
- `itest.sh` requires the new call suite by name (not only the login suite); a skipped call test fails
  the run. Plus the signed, sandboxed, Finder-launched acceptance (§6) — `swift test` cannot prove
  packaging, entitlements or permission prompts.
- Mutations per the usual rule on the queue (inline WebRTC call), the fence, weak attachment,
  camera-off (disable instead of stop), and mid mapping.

## 6. Acceptance (human + measured)
1. A Finder-launched Brook.app joins a call on the test server with a GTK participant: both see and
   hear each other; mute and camera toggle show on the other side; leave cleans up both rosters.
2. **Echo check with laptop speakers** (no headphones), 2 minutes: the GTK side reports no echo of its
   own voice. If AEC3 fails this, the follow-up is a custom `RTCAudioDevice` with Apple voice
   processing (or LiveKit's fork) — recorded, not silently accepted.
3. CPU on an M-series Mac during a 2-party 720p call noted in the PR (Activity Monitor), for the
   "respect the hardware" goal.

## 7. Failure modes
| Risk | Mitigation |
|---|---|
| Generated Swift breaks under Swift 6 | own target in Swift 5 mode (spike-proven); hand-written code stays Swift 6 |
| No UDP ICE in sandbox | `network.server` (spike-proven) |
| Third-party binary changes | exact version + checksum pin; rebuild from the public script if needed |
| x86_64 code shipped | lipo-thin build phase, verified with `lipo -archs` on the built app |
| AEC3 echo on speakers | acceptance §6.2 measures it; fallback path named |
| Camera/mic prompts at a surprising time | only on join-with-publish; denial degrades, never crashes |
| Engine callbacks on arbitrary threads racing UI | Mutex-guarded state; UI via main actor |

## 8. Review log

**Round 1 — Codex + Vibe (Heavy).** Codex requested changes; all accepted: engine-owned serial queue
(WebRTC setters can block; core calls the sync methods inline); camera-off stops capture (disabling the
track left the camera and its indicator on), mute's open-microphone behaviour stated; engine `closed`
completion as the capture-stop barrier, fence set first and checked at entry/resume/install, weak
handle attachment (a strong one defeats drop-to-leave); stronger tests (decoded frames, received audio,
selected pair, capture state, both-way loopback, held-op races, attachment race, mid reuse, itest
requires the call suite, Finder acceptance kept); package split wiring (generator path, `BrookMedia`
product, `@retroactive` redaction, round-trip test); explicit transceiver directions; forward
`set_ice_servers`; permissions resolved before join, microphone first, listen-only cannot be promoted.
Vibe: every end path through `close()` — already core's single `finish()`, stated; the one missing
path, app quit, added. `network.server` exposure documented. Thinned framework re-signed as the
embedded copy, verified on the Release bundle. Echo acceptance gates the merge. "Tear down the
subscribe PC when no streams remain" — rejected: the proven model keeps the PC across an empty re-offer
(it is reused when someone joins again).
