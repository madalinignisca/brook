# sfu — media server (Janus + VideoRoom)

The real-time **media router** (SFU). See [../../docs/MEDIA.md](../../docs/MEDIA.md) for the full rationale.

## What it does
- Receives **one** encoded stream per sender; **forwards** copies to subscribers.
- **Never transcodes** → server CPU stays low; all encoding is client-side.
- Picks per-receiver **simulcast/SVC** layers; handles RTP/RTCP, retransmits, keyframe requests.
- Is the public reachable endpoint → minimizes NAT/TURN pain.

## Why Janus over mediasoup
- Standalone C daemon driven from FastAPI over a **documented API** (no Node service to run).
- **VideoRoom** plugin ships room/simulcast/recording logic → faster to a working call.
- Friendlier to our **bring-your-own native `webrtcbin`** client.
- mediasoup stays the fallback for finer routing control / aggressive horizontal scaling.

## Integration (api-proxied — decided)
- **`api` owns all Janus sessions/handles** and is the only thing that talks to the Janus API. Clients send SDP/ICE over their WSS to `api`, which proxies to Janus. See [../../docs/ARCHITECTURE.md](../../docs/ARCHITECTURE.md) §Signaling model.
- Clients do **not** present a token to Janus; `api` authorizes joins from the smartChat session.
- Only **SRTP/DTLS media** flows client ↔ SFU directly (never via `api`).
- **Janus Admin API is internal-only** — never exposed by the gateway.
- **TURN:** `coturn` is part of the media architecture, not an afterthought — see [../../docs/MEDIA.md](../../docs/MEDIA.md) §TURN (ICE server distribution, credentials, ports, TLS/TCP fallback).

## This directory will hold
Janus config (`janus.jcfg`, `janus.plugin.videoroom.jcfg`), container setup, and notes on the VideoRoom message flow + session lifecycle owned by `api`.
