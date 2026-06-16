# Media: WebRTC, the SFU, hardware encode, and the Raspberry Pi target

> The hardest part of the whole product. This document is the contract between `core` (signaling) and each client's platform media engine.

## 1. Topology recap

We use an **SFU** (Janus + VideoRoom). Each participant uploads **one** encoded stream; the SFU forwards copies to subscribers and **never transcodes**. Therefore:

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

Default: **H.264 simulcast (e.g. 720p + 360p)** on capable hardware; **single-layer** on constrained devices like the Pi.

**Janus VideoRoom config (to pin down in Phase 4):** publishers declare simulcast (rid-based or legacy `simulcast`); the room advertises codecs (H.264 with the agreed profile/level-id, VP8 fallback). Subscribers request a substream/temporal layer; `core` sends `LayerHint`s and the SFU switches layers per receiver. Document the exact `videoroom` room parameters (`videocodec`, `h264_profile`, simulcast settings) alongside the engine work.

## 6. The Raspberry Pi 4B target (optimization litmus test)

> If a call works well on a Pi 4B running an up-to-date GTK4 desktop, the optimization goal is met.

This is feasible **because** of the architecture (native GTK4 UI, no Electron, Rust core, HW H.264):

**Realistic Pi 4B profile**
- **Client:** the GNOME/Linux client (GTK4 + libadwaita) — identical codebase, no special build.
- **Encode:** `v4l2h264enc` (VideoCore HW H.264). The Pi 4B's H.264 *encoder* is modest, so:
  - Target **720p30 single-layer** for the outgoing camera (no simulcast on the Pi — it can't afford the extra encodes).
  - **Screen share at reduced framerate** (e.g. 1080p @ 5–10 fps — screens are mostly static; this is cheap and crisp).
- **Decode:** HW H.264 decode handles incoming streams up to 1080p comfortably.
- **Pipeline:** keep frames in GPU memory (DMA-BUF) from `pipewiresrc`/camera → `v4l2h264enc` → `webrtcbin`; avoid CPU copies.
- **Constraints to respect:** cap the number of *decoded* incoming video tiles (e.g. show only the active speaker + thumbnails) to bound decode load; prefer audio-only fallback under pressure.

**Why it should hold:** the Pi only ever encodes **one** stream (SFU forwards it), decodes a **bounded** number, and renders with native GTK4. No web engine, no transcoding, GC-free Rust core. The risks are the Pi's weak H.264 *encoder* and thermals — mitigated by single-layer 720p and framerate caps.

> ⚠️ **This is a hypothesis, not a measured result.** The Pi 4B's VideoCore H.264 *encoder* is modest and thermally constrained, and `v4l2h264enc` quality/throughput vary by firmware. Treat the targets above as a starting profile to **validate with the acceptance test before committing** — adjust resolution/fps (or fall back to audio-only) based on real numbers. Active cooling likely required for sustained calls.

**Pi acceptance test:** 1:1 call, camera 720p30 + screen share, sustained 10 min, CPU headroom remaining and no thermal throttle. Then a 3-person channel call (active-speaker view).

## 7. Open decisions

- Per-client: GStreamer vs. platform-native WebRTC (esp. iOS).
- TURN server (`coturn`) needed for restrictive NATs even with an SFU — add in the calls phase.
