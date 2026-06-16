# Media: WebRTC, the SFU, hardware encode, and the Raspberry Pi target

> The hardest part of the whole product. This document is the contract between `core` (signaling) and each client's platform media engine.

## 1. Topology recap

We use an **SFU** (Janus + VideoRoom). Each participant uploads **one encoded stream per published track** (camera, screen, mic); the SFU forwards copies to subscribers and **never transcodes**. Therefore:

- **All encoding/decoding happens on the client.** Hardware acceleration is a *client* concern; the SFU choice does not affect it.
- The SFU is the public, reachable endpoint, so clients connect *to it* — this largely removes the NAT/TURN burden of peer-to-peer.

## 2. Encryption (always on)

WebRTC mandates media encryption: **SRTP keyed by DTLS**. There is no unencrypted mode. This is **hop-by-hop** (client ↔ SFU), so the operator's server can access media — **accepted by design** (self-hosted, own-your-data; **E2EE is a non-goal**). See [SECURITY.md](SECURITY.md) §2.

## 3. Codec choice: H.264 baseline

We standardize on **H.264** as the primary call codec because it has the **broadest hardware encode/decode coverage across all five platforms and the Raspberry Pi**:

| Platform | HW encode path | HW decode |
|---|---|---|
| Linux x86 (Intel/AMD) | VAAPI (`vah264enc`) | VAAPI |
| **Raspberry Pi 4B** | **V4L2 M2M (`v4l2h264enc`)** | V4L2 / HW H.264 decode |
| macOS / iOS | VideoToolbox | VideoToolbox |
| Windows | Media Foundation / NVENC / AMF / QSV | DXVA |
| Android | MediaCodec | MediaCodec |

- **VP8** is kept as a **software fallback** (cheap to encode at low resolution) for devices lacking H.264 HW encode.
- VP9/AV1-SVC are future options where HW supports them (better layering, but narrower HW encode coverage today).

## 3b. Audio (Opus + echo cancellation) — do not forget this

The video story above is only half a call. Audio:

- **Codec: Opus** (WebRTC standard; software encode/decode is cheap everywhere, including the Pi).
- **Acoustic Echo Cancellation (AEC) is mandatory** for speaker calls (otherwise remote parties hear themselves). Plus noise suppression + auto-gain. On native GStreamer this is software DSP (`webrtcdsp`/`webrtcechoprobe`), which **costs real CPU** — and that cost lands on the **Pi 4B**, directly competing with H.264 encode for its budget.
- Prefer the platform's **hardware/OS AEC** where available (PipeWire/WirePlumber echo-cancel module on Linux, VoiceProcessingIO on Apple, `AcousticEchoCanceler` on Android) before falling back to software DSP.
- **Pi impact:** the acceptance test ([§6](#6-the-raspberry-pi-4b-target-optimization-litmus-test)) must include audio with AEC running, and the CPU budget must account for it — not just video encode.
- The `MediaEngine` trait's `MediaSource` includes **mic**; audio capture/AEC/encode is part of each platform implementation.

## 4. The media engine abstraction

`core` owns **signaling only** (negotiating with Janus over the WSS relay). The **media engine** — capture, encode, transport, decode, render — is per-platform behind a common interface so `core` can drive it uniformly:

```
core (Rust)  ──trait MediaEngine──►  platform implementation
   negotiate SDP/ICE                 - start/stop capture (camera, screen)
   add/remove tracks                 - encode (HW) → RTP
   pick simulcast layer hints        - send/recv via SRTP/DTLS
                                      - decode (HW) → render to a native surface
```

### `MediaEngine` contract (sketch — finalize before Phase 4)
`core` drives signaling and calls into the engine; the engine reports back via events. Indicative async interface:

```rust
trait MediaEngine {
    async fn join(&self, room: RoomConfig) -> Result<()>;      // create transports
    async fn leave(&self) -> Result<()>;
    async fn publish(&self, src: MediaSource) -> Result<TrackId>; // camera | screen | mic
    async fn unpublish(&self, t: TrackId) -> Result<()>;
    async fn subscribe(&self, remote: TrackId, layer: LayerHint) -> Result<()>;
    async fn apply_remote_sdp(&self, sdp: Sdp) -> Result<()>;
    async fn add_remote_ice(&self, c: IceCandidate) -> Result<()>;
    // events out: LocalSdp, LocalIce, TrackAdded/Removed, RenderTarget, Stats
    fn events(&self) -> EventStream<MediaEvent>;
}
```
`RenderTarget` hands the native layer a surface/handle to draw decoded video into (GTK `Paintable`, `CALayer`, `SurfaceView`, …). Screen capture is initiated by the engine via the platform path (Wayland portal → `pipewiresrc`, ScreenCaptureKit, MediaProjection).

### Primary strategy: GStreamer `webrtcbin` everywhere it fits
GStreamer is cross-platform and gives **explicit control of the hardware encoder element** per platform (`vah264enc`, `v4l2h264enc`, `nvh264enc`, …) plus **zero-copy DMA-BUF** from capture to encoder. It is the natural fit for **Linux, the Pi, and Windows**, and works on Android/macOS too.

- **Screen capture on Wayland** (GNOME/Pi): via `libportal` (xdg-desktop-portal **ScreenCast** portal) → `pipewiresrc` → encoder. The compositor's picker handles "whole screen / monitor / single window" and user consent; the app just receives the PipeWire node. Same client code works on GNOME (`xdg-desktop-portal-gnome`) and wlroots.

### Platform-native escape hatch
On **iOS** (and possibly macOS/Android) GStreamer may be too heavy or awkward vs. the platform's own WebRTC + VideoToolbox/MediaCodec. The `MediaEngine` trait lets such a client swap in a native implementation **without changing `core`**. Decision is per-client; documented in each client README.

## 5. Simulcast / SVC and the encoder

To let the SFU serve different viewers different qualities, the sender either:
- **Simulcast**: encode the same source 2–3× at different resolutions → **multiplies encoder load** → exactly where hardware encode pays off; or
- **SVC** (VP9/AV1): one scalable encode the SFU peels layers from → lighter encode, narrower HW support.

**Baseline = single-layer H.264.** Simulcast with H.264 is genuinely tricky — RID/profile-level-id negotiation must line up across Janus, GStreamer `webrtcbin`, hardware encoders, and any browser/Safari peer, and it multiplies encoder load. So:
- **MVP ships single-layer H.264** (one encode per published track). Quality adapts via bitrate/resolution renegotiation, not layers.
- **Group-call gotcha:** in a non-transcoding SFU, *every receiver gets exactly what the sender encodes*. With no simulcast, a strong sender (e.g. a Mac pushing 1080p high-bitrate) can swamp weak receivers (Pi, poor mobile links). **MVP mitigation: enforce a conservative per-publisher max bitrate/resolution in group rooms** (e.g. ≤ ~1 Mbps / 720p), set via the Janus VideoRoom room config and/or sender-side caps in `core`, so the lowest-common-denominator stays playable. Lift it only once simulcast/SVC lands.
- **Simulcast is a Phase-4 spike**, not assumed working: prove H.264 simulcast end-to-end (Janus VideoRoom ⇄ GStreamer) with measured interop before relying on it; otherwise stay single-layer (or evaluate VP9/AV1-SVC where HW allows).

**Janus VideoRoom config (to pin down in Phase 4):** room advertises codecs (H.264 with an explicitly agreed `profile-level-id`, VP8 fallback). If/when the simulcast spike succeeds: publishers declare simulcast (rid-based or legacy `simulcast`), subscribers request a substream/temporal layer, and `core` sends `LayerHint`s. Document the exact `videoroom` parameters (`videocodec`, `h264_profile`, simulcast settings).

## 6. The Raspberry Pi 4B target (optimization litmus test)

> If a call works well on a Pi 4B running an up-to-date GTK4 desktop, the optimization goal is met.

This is feasible **because** of the architecture (native GTK4 UI, no Electron, Rust core, HW H.264):

**Realistic Pi 4B profile**
- **Client:** the GNOME/Linux client (GTK4 + libadwaita) — identical codebase, no special build.
- **Encode:** `v4l2h264enc` (VideoCore HW H.264). The Pi 4B's H.264 *encoder* is modest, so:
  - Target **720p30 single-layer** for the outgoing camera (no simulcast on the Pi — it can't afford the extra encodes).
  - **Screen share at reduced framerate** (e.g. 1080p @ 5–10 fps — screens are mostly static; this is cheap and crisp).
- **Decode:** HW H.264 decode handles incoming streams up to 1080p comfortably.
- **Pipeline:** aim to keep frames in GPU memory (DMA-BUF) from `pipewiresrc`/camera → `v4l2h264enc` → `webrtcbin`; avoid CPU copies.
  - ⚠️ **DMA-BUF retention risk (primary Pi unknown):** GStreamer's WebRTC RTP payloader (`rtph264pay`) often **can't consume DMA-BUF directly** and forces a CPU memory-map copy — which would erase the zero-copy win on the Pi. **Spike this first:** prove `v4l2h264enc ! rtph264pay ! webrtcbin` retains DMA-BUF (or measure the memcpy cost) **before** committing to the Pi target in Phase 4.
- **Constraints to respect:** cap the number of *decoded* incoming video tiles (e.g. show only the active speaker + thumbnails) to bound decode load; prefer audio-only fallback under pressure.

> **Encode budget — important:** "one encode per *published track*" is the rule, so **camera + screen share simultaneously = two H.264 encodes**, which likely exceeds the Pi 4B's encoder. On the Pi, treat camera and screen share as **mutually exclusive by default** (sharing the screen pauses the camera), so the Pi encodes **one** track at a time. Simultaneous camera+screen on the Pi is only enabled if measurements show headroom.

**Why it should hold:** with the one-track-at-a-time rule the Pi encodes **a single** H.264 stream (SFU forwards it), decodes a **bounded** number, and renders with native GTK4. No web engine, no transcoding, GC-free Rust core. The risks are the Pi's weak H.264 *encoder* and thermals — mitigated by single-layer 720p, framerate caps, and camera-or-screen.

> ⚠️ **This is a hypothesis, not a measured result.** The Pi 4B's VideoCore H.264 *encoder* is modest and thermally constrained, and `v4l2h264enc` quality/throughput vary by firmware. Treat the targets above as a starting profile to **validate with the acceptance test before committing** — adjust resolution/fps (or fall back to audio-only) based on real numbers. Active cooling likely required for sustained calls. The whole Pi target is **conditional on these measurements**.

**Pi acceptance test:** 1:1 call, **camera 720p30 *or* screen share** (one encode), sustained 10 min, CPU headroom remaining and no thermal throttle. Then a 3-person channel call (active-speaker view, decode-bounded).

## 6a. TURN (`coturn`) — part of the architecture, not an afterthought

An SFU removes *peer-to-peer* NAT pain (clients connect to the SFU), but a client behind a restrictive firewall still may not reach the SFU's UDP media ports. TURN is required for those:

- **`coturn`** relays media when direct UDP fails. Offer **TURN over UDP, TCP, and TLS/443** so it works through hostile firewalls that only allow 443.
- **ICE servers** (STUN/TURN URLs + **short-lived TURN credentials**, e.g. HMAC `rest` credentials minted by `api`) are delivered to the client during call setup over the WS.
- **Ports:** open the SFU media UDP range and the coturn relay range; document them in deploy.
- **Fallback order:** host/srflx (direct) → TURN/UDP → TURN/TCP → TURN/TLS:443.
- Stand up in **Phase 4** alongside calls.

## 7. Open decisions

- Per-client: GStreamer vs. platform-native WebRTC (esp. iOS).
- H.264 simulcast viability (Phase-4 spike) vs. staying single-layer / SVC.
