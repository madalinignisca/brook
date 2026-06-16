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

## Integration
- `api` mints **room-scoped, short-lived join tokens**; clients present them to join.
- Media is **SRTP/DTLS**, client ↔ SFU directly (never via `api`).
- `coturn` to be added in the calls phase for restrictive NATs.

## This directory will hold
Janus config (`janus.jcfg`, `janus.plugin.videoroom.jcfg`), container setup, and notes on the VideoRoom message flow.
