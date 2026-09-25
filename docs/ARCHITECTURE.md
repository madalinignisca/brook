# Architecture

> Read alongside [CLIENT_PHILOSOPHY.md](CLIENT_PHILOSOPHY.md), [MEDIA.md](MEDIA.md), and [SECURITY.md](SECURITY.md).

## 1. System overview

Brook is a classic **client / server** system with a **dedicated media server (SFU)** for real-time audio/video, and **object storage** for files. Clients never talk to each other directly; they talk to the services.

```
                         ┌──────────────────────────────────────────────┐
                         │                  SERVER SIDE                   │
                         │                                                │
   native clients        │   ┌─────────┐   TLS    ┌──────────────────┐    │
   (5 platforms,         │   │ gateway │◄────────►│  api (FastAPI)   │    │
    shared Rust core)    │   │ (Caddy) │   HTTPS   │ chat/channels/   │    │
        │                │   │  TLS    │   WSS     │ files/bots/auth  │    │
        │  HTTPS/WSS     │   │ term.   │           └───────┬──────────┘    │
        ├───────────────►│   └─────────┘                   │              │
        │                │        ▲                 ┌──────┴──────┐       │
        │                │        │                 │ PostgreSQL  │       │
        │                │        │                 └─────────────┘       │
        │                │        │                 ┌─────────────┐       │
        │                │        │                 │ files on    │       │
        │                │        │                 │ local disk  │       │
        │                │        │                 │ (via api)   │       │
        │                │        │                 └─────────────┘       │
        │  SRTP/DTLS     │   ┌──────────────────────────────────────┐     │
        └───────────────────►│ sfu (Janus + VideoRoom plugin)       │     │
           media + screen │   │ forwards media; never transcodes     │     │
                         │   └──────────────────────────────────────┘     │
                         └──────────────────────────────────────────────┘
```

## 2. Components

| Component | Tech | Responsibility |
|---|---|---|
| **gateway** | Caddy | Single public entry point; terminates **TLS** (auto Let's Encrypt) for HTTPS + WSS; routes to `api`. The **Janus Admin API is never routed publicly** (internal network only). |
| **api** | Python + FastAPI | Auth, users, channels/DMs, message persistence + fan-out (WebSocket), file metadata + presigned URL minting, bot registry, webhook in/out, **owns Janus sessions & proxies call signaling**. |
| **sfu** | Janus + VideoRoom | Real-time media **router** (SFU). Receives one upstream per sender, forwards to subscribers. **No transcoding.** See [MEDIA.md](MEDIA.md). |
| **storage** | Local filesystem | Attachment bytes under `/var/lib/brook/files`, written and served by `api` (owner decision: object storage earns its keep only for horizontal scaling). |
| **db** | PostgreSQL | Users, channels, membership, messages, files metadata, bots, tokens. |
| **core** | Rust library | Shared client logic: networking, protocol, state, file transfer, **call signaling**, crypto. Compiled into every client. |
| **clients** | per-platform native | Thin native UI over `core`. One per OS. |

## 3. The three transport planes

Brook deliberately separates concerns into three planes, each with different properties (this is why the design stays simple):

1. **Control plane** — REST over **HTTPS** (`client → api`): login, history, channel ops, file metadata, bot registration. Request/response.
2. **Realtime plane** — WebSocket over **WSS** (`client ↔ api`): live messages, presence, typing, and **call signaling relay** (SDP/ICE to/from Janus). Bidirectional, low-latency.
3. **Media plane** — **SRTP/DTLS** (`client ↔ sfu`): the actual audio/video/screen-share packets. Encrypted by WebRTC itself. High-bandwidth, never touches `api`.

File bytes go **client ↔ api** over HTTPS (streamed uploads with a hard cap, ranged downloads).

> Key consequence: media, the bandwidth-heavy path, bypasses the `api`. Files go through it, which is fine for a family-sized server; a deployment that must scale out would move them to object storage.

## 4. Why an SFU, and what it does

The SFU is a **selective forwarder**, not a mixer. Each participant uploads **one encoded stream per published track**; the SFU routes copies to subscribers. It does **not** decode or re-encode — so **all encoding happens on the client**, which is why hardware encode is purely a client concern (see [MEDIA.md](MEDIA.md)). This keeps server CPU low.

### Signaling model (decided): `api`-proxied
Clients **never** speak the Janus API directly. **`api` owns all Janus sessions/handles** and is the only thing that talks to the Janus API. The client sends SDP/ICE over its **WSS** to `api`, which proxies to Janus and relays answers/candidates back. The **only** thing that flows client↔SFU directly is the **SRTP/DTLS media** (the media plane). Consequences:
- "Authorization to join a call" = the client's **Brook session** (checked by `api`); there is no separate Janus token the *client* presents. If Janus token auth is enabled, `api` manages those tokens server-side.
- `api` owns Janus session/handle lifecycle: create on join, ICE trickle relay, renegotiation, and teardown on leave/disconnect/idle-cleanup (see [SECURITY.md](SECURITY.md) §7).
- The **Janus Admin API is internal-only** — never routed publicly by the gateway.

We chose **Janus + VideoRoom** over mediasoup because: it is a standalone daemon driven from FastAPI over a documented API (no Node service), it is friendlier to a **bring-your-own** native `webrtcbin` peer, and its VideoRoom plugin ships room/simulcast/recording logic so we reach a working call sooner. mediasoup remains the fallback if we later need its finer routing control or horizontal-scaling model.

## 5. Bots & webhooks (core features 6 & 7)

- **Inbound (external → channel):** a registered bot has a secret; external systems POST messages to `api`, signed (HMAC). `api` posts them into the channel as that bot participant.
- **Outbound (`/botname message`):** a user typing `/botname …` in a channel triggers `api` to POST (signed) to that bot's registered URL. The bot may reply via its inbound webhook.
- Security: HMAC signing both directions, HTTPS-only targets, and **SSRF protection** on user-supplied URLs. See [SECURITY.md](SECURITY.md).

## 6. Client architecture (summary)

Every client = **native UI** + **shared Rust `core`**. The core exposes an async, UI-agnostic API (observable state + commands); the native layer renders it with the platform toolkit and feeds platform capabilities (file picker, notifications, screen capture) back in. Full rationale and the per-platform binding strategy: [CLIENT_PHILOSOPHY.md](CLIENT_PHILOSOPHY.md).

## 7. Data & protocol

- Entities and relationships: [DATA_MODEL.md](DATA_MODEL.md)
- Wire protocol (WS event types + REST endpoints): [PROTOCOL.md](PROTOCOL.md)

## 8. Deployment

Local dev is a single `docker-compose` bringing up Postgres, Janus, Caddy, and the api (attachments on a named volume). See [../deploy/](../deploy/). Production is the same components behind Caddy with real certificates.

### Realtime state & scaling (decided)
The WebSocket layer is **stateful** (presence, channel fan-out, call signaling, offline replay), so `api` is **not** trivially stateless:
- **MVP: single-node `api`** (a small business runs one node). The WS hub, presence, and Janus session ownership live in-process. Simple and correct.
- **Scale-out (later): sticky WS routing + a pub/sub bus** (Redis or NATS) so any node can fan-out messages/presence to sockets it doesn't hold. Call signaling stays **node-affine** (the node owning a call's WS owns its Janus session). Presence/session state moves to shared storage (Redis).
- This is an explicit decision, not "stateless + horizontal" (which would contradict the realtime design).
