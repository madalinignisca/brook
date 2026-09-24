# Apple call engine + macOS call UI (C2/C3)

> Status: **draft** — awaiting review · 2026-09-24
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
`bindings/apple/swift/BrookCore/Package.swift` gains:
- target **`BrookCoreGenerated`** — the UniFFI Swift, **Swift 5 language mode** (spike: it does not
  compile in 6); depends on the `BrookCoreFFI` binary target.
- target **`BrookCore`** — hand-written, Swift 6, depends on `BrookCoreGenerated` and re-exports it.
- target **`BrookMedia`** — the libwebrtc engine, Swift 6, depends on `BrookCore` and
  `.binaryTarget(name: "WebRTC", url: <stasel 153.0.0 zip>, checksum: "3e3a8946…2b78f")`.
- The macOS app embeds `WebRTC.framework`; a build phase **thins it to arm64** (`lipo -thin arm64`)
  before signing, so the shipped app contains no x86_64 code (the prebuilt slice is universal).

### 3.3 `BrookMedia.WebRTCEngine` (implements `MediaEngine`)
- One `RTCPeerConnectionFactory` (default encoder/decoder factories → H.264 via VideoToolbox,
  VP8 fallback, Opus). Unified Plan.
- **Publish PC** (sendonly): audio track from the default audio device module (WebRTC's AEC3 on
  macOS — spike-noted: not Apple voice processing; measured in §6), video from `RTCCameraVideoCapturer`
  on the default camera at ≤ 720p30 (contract cap 1.5 Mbps / 720p). `create_publish_offer` = create
  offer + set local description; capture start is bounded (≤ 5 s) and fenced by `close()`.
- **Subscribe PC** (recvonly), created lazily on the first `apply_subscribe_offer`; the same PC is
  kept across re-offers (proven model: empty re-offer after leave keeps the PC).
- Local ICE candidates and fatal errors are pushed into the `FfiCallHandle` once it exists; before
  that they are buffered in the engine (the GTK adapter does the same: join returns after the engine
  may already have produced candidates).
- Remote tracks → a `mid → RTCVideoTrack/RTCAudioTrack` map published to the UI (`@Observable`),
  keyed by the latest `streams` mapping; a re-offer that reuses a mid rebinds the tile.
- `set_local_media`: `track.isEnabled` for audio/video (no renegotiation).
- `close()`: stops capture, closes both PCs, and sets a fence flag checked when every async op
  resumes (a capture that finishes starting after close is stopped immediately and the op errors).
- Threading: WebRTC callbacks arrive on its signaling thread; the engine's state is guarded by a
  `Mutex`; UI updates hop to the main actor; nothing blocks the Rust worker threads that call the
  sync methods.

### 3.4 Permissions and entitlements (macOS)
- Entitlements: `app-sandbox`, `network.client`, **`network.server`** (UDP ICE — §2),
  `device.camera`, `device.audio-input`. Nothing else.
- Info.plist: `NSCameraUsageDescription`, `NSMicrophoneUsageDescription` ("…for calls in Brook").
- Permission is requested **only when the user joins a call with publish**, never at launch. Denied
  camera → join audio-only (video track absent) and say so; denied microphone → join listen-only
  (`publish = false`) and say so.

### 3.5 macOS UI (C3, minimal)
- After sign-in: a `NavigationSplitView` sidebar of channels (`list_channels`), each showing
  "● Call · N" from `ChannelCall` events; a **Join call** button.
- Call window: a grid of video tiles (`RTCMTLNSVideoView` wrapped in `NSViewRepresentable`) labelled
  from the roster; self-view; controls: mute, camera, leave (⌘⇧M, ⌘⇧V, ⌘W); status banner for
  Reconnecting / Ended(reason). Closing the window awaits `leave()` (bounded) before it closes.

## 4. Not doing
- iOS app, CallKit, push (later; the engine is written to be shared).
- Screen share, simulcast, TURN, Apple voice-processing AEC (possible custom ADM later — §6 decides).
- Chat UI (messages) — only the channel list needed to join a call.

## 5. Tests
- Rust (bindings): the call API round-trips through the FFI types; adapter maps engine errors.
- Swift unit: `WebRTCEngine` against a **loopback**: publish PC's offer applied as a subscribe offer
  on a second engine instance (no server) — both reach connected, a remote video track arrives,
  `close()` fences a capture start held open, `set_local_media` toggles `isEnabled`.
- Swift integration (itest, shared test server): join → Connected; publish answer applied; a
  subscribe offer from a second participant (browser harness or GTK) answered; leave → Ended(Left).
- Mutations per the usual rule on the engine's fence, buffering-before-handle, mid→track mapping.

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
