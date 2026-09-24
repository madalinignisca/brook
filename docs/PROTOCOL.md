# Wire protocol

> Draft v0. Transport is split: **REST/HTTPS** for request/response, **WebSocket/WSS** for realtime + call signaling relay. All payloads JSON (a binary/CBOR option may come later for efficiency).

## 1. REST (control plane) — `https://<host>/api/v1`

| Method & path | Purpose |
|---|---|
| `GET  /auth/methods` | which methods this deployment enabled (local/oidc/ldap) |
| `POST /auth/login` | local credentials → `{access_token, refresh_token}` or `{totp_required}` |
| `POST /auth/totp` | login-time TOTP code → tokens; `POST /auth/totp/enroll` to set up |
| `GET  /auth/oidc/start` | begin OIDC (Auth Code + PKCE) in system browser |
| `GET  /auth/oidc/callback` | provider redirect; api exchanges provider code, issues a short-lived Brook code, redirects to the app |
| `POST /auth/oidc/exchange` | app exchanges the Brook code + PKCE verifier → `{access_token, refresh_token}` |
| `POST /auth/ldap` | LDAP bind credentials → tokens |
| `POST /auth/refresh` | refresh → new access token (rotates refresh token) |
| `POST /auth/logout` | revoke refresh token |
| `GET  /health` | liveness/readiness (also on `sfu`; unauthenticated) |
| `GET  /me` · `PATCH /me` | current user · update profile/avatar |
| `GET  /channels` | channels/DMs the user belongs to |
| `POST /channels` | create channel |
| `GET /channels/{id}` · `PATCH /channels/{id}` · `DELETE /channels/{id}` | get / rename-topic / delete |
| `GET /channels/{id}/members` · `POST` · `DELETE /channels/{id}/members/{uid}` | list / add / remove member |
| `GET  /channels/{id}/messages?before=&after=&limit=` | history: `before=<id>` (back-paginate) or `after=<id>` (**forward-sync** missed messages on reconnect) |
| `POST /channels/{id}/messages` | **send a message (the only send path)**; server persists then fans out via WS |
| `PATCH /messages/{id}` · `DELETE /messages/{id}` | edit / soft-delete (author or channel owner) |
| `POST /channels/{id}/files` | begin upload → **S3 POST Policy** (`{url, fields}` with a `content-length-range`) + `file_id` (state `pending`) |
| `POST /files/{id}/commit` | finalize: server confirms the object exists + type ok → state `committed` (attachable) |
| `GET  /files/{id}` · `DELETE /files/{id}` | request **presigned GET** → `{download_url}` · delete |
| ~~`POST /channels/{id}/calls`~~ | superseded: calls are joined over the WS with `call.join` (§3), one path, no REST step |
| `POST /devices` · `DELETE /devices/{id}` | register/unregister an APNs/FCM push token (mobile) |
| `GET  /bots` · `POST /bots` | list / register bots (returns signing secret once) |
| `GET /bots/{id}` · `PATCH /bots/{id}` · `DELETE /bots/{id}` | get / update (url, regen secret) / delete |
| `POST /channels/{id}/bots` · `DELETE /channels/{id}/bots/{bot}` | add / remove bot from channel |
| `POST /bots/{id}/webhook` | **inbound** webhook: external posts as bot (HMAC-signed) |

## 2. WebSocket (realtime plane) — `wss://<host>/ws`

**Authentication:** the access token is sent in the **first message** after the socket opens (an `auth` command), **not** as a query parameter (query strings leak into logs/proxies). The server rejects (closes) the socket if the first frame is not a valid `auth` within a short timeout. Tokens are re-validated; an expired token closes the socket and the client refreshes + reconnects.

Messages are tagged envelopes:

```json
{ "type": "<event>", "id": "<uuid>", "ts": "<iso8601>", "data": { ... } }
```

### Server → client events
| type | data |
|---|---|
| `message.new` | message object (channel_id, author, body, attachments) |
| `message.edit` / `message.delete` | message id + change |
| `presence.update` | user id, online/away/offline |
| `typing` | channel id, user id |
| `channel.update` | membership / metadata change |
| `bot.message` | message authored by a bot participant |
| `call.*`, `channel.call` | call signaling events, see §3.4 |

### Client → server commands
> **Sending messages is REST-only** (`POST /channels/{id}/messages`), never a WS command — one send path avoids races between HTTP retries and WS reconnect-replay, and simplifies dedup. The WS carries only ephemeral signals (typing, call) and **receives** fan-out.

| type | data |
|---|---|
| `auth` | access token (**required first frame**, see above) |
| `typing` | channel id |
| `call.*` | call signaling commands, see §3.3 |
| `slash.command` | channel id, raw text (e.g. `/botname hello`) → triggers outbound webhook |

## 3. Call signaling (over the WS, relayed to Janus)

> **Contract v1 (2026-09-24).** The shape the server (`services/api`) and the client
> (`core`, via FFI) build against in parallel. Changing a message here is a
> coordinated change: announce it to the other side before it lands.

### 3.1 Model

- **One call per channel.** A call is keyed by `channel_id`. The first `call.join`
  starts it; it ends when the last participant leaves (or the cleanup timeout in
  [SECURITY.md](SECURITY.md) §7 fires). **Authorization = channel membership**,
  checked by `api` on every call command.
- **Two PeerConnections per participant**, both terminated by the SFU:
  - **publish** — `sendonly`: the participant's mic, camera, and later screen.
    The **client offers**, the server answers.
  - **subscribe** — `recvonly`: **all** remote streams in one PeerConnection.
    The **server offers**, the client answers. Whenever the set of remote streams
    changes, the server sends a **new offer** on the same PeerConnection (renegotiation).
- `api` owns every Janus session and handle; the client never sees Janus ids or
  Janus messages (ARCHITECTURE.md §Signaling model). Media (SRTP/DTLS) flows
  client ↔ SFU directly.
- Codecs: **Opus** audio, **H.264** video (constrained baseline, `profile-level-id=42e01f`)
  with VP8 as negotiated fallback. See [MEDIA.md](MEDIA.md).

### 3.2 Envelope, correlation, errors

Every frame in both directions is the §2 envelope `{type, id, ts, data}`.

- A client **command** carries a fresh `id`. The server's **direct reply** to it
  carries `re: <that id>` (sibling of `data`), so the client can match reply to
  request. Unsolicited server events have no `re`.
- Every command with a reply in §3.3 gets **exactly one** reply: its success frame
  **or** an `error`, never both, never none. Clients may await it with a timeout
  (10 s is ample). `call.ice` has no reply.
- A failed command gets `{"type":"error","re":"<id>","data":{"code":"…","message":"…"}}`.
  `message` is for logs, not UI. Codes:

| code | meaning |
|---|---|
| `invalid` | malformed frame / missing field / bad SDP |
| `not_member` | caller is not a member of the channel |
| `not_in_call` | command refers to a call the caller hasn't joined |
| `call_full` | participant limit reached (§3.6) |
| `bad_state` | command out of order (e.g. `call.publish` before `call.joined`) |
| `stale` | `call.subscribe.answer` for a `version` that is no longer the latest; discard it and answer the newer offer |
| `sfu_unavailable` | Janus unreachable or refused; retry later |

### 3.3 Client → server commands

| type | data | reply |
|---|---|---|
| `call.join` | `{channel_id}` | `call.joined` |
| `call.publish` | `{call_id, sdp}` (publish-PC **offer**) | `call.publish.answer` |
| `call.subscribe.answer` | `{call_id, version, sdp}` (subscribe-PC **answer** to the offer with that `version`) | `call.ok` or `error: stale` |
| `call.ice` | `{call_id, pc: "publish"\|"subscribe", candidate}` | none (fire-and-forget) |
| `call.media` | `{call_id, audio: bool, video: bool}` (mute state as the user sees it) | `call.ok` |
| `call.leave` | `{call_id}` | `call.ok` |
| `call.resume` | `{call_id}` (after a WS reconnect, §3.5) | `call.joined` |

`candidate` is `{candidate, sdpMid, sdpMLineIndex}` as produced by WebRTC, or `null`
for end-of-candidates.

**Who buffers ICE.** The server relays client candidates to the SFU immediately;
the SFU accepts them before or after the SDP. The **client** buffers any
server → client `call.ice` that arrives before it has applied the matching remote
description, and applies them after. (The SFU runs ICE-lite and normally puts
its candidates in the SDP, so server → client trickle is rare but allowed.)

### 3.4 Server → client events

| type | data | when |
|---|---|---|
| `call.joined` | `{call_id, channel_id, self: {participant_id}, participants: [Participant]}` | reply to `call.join` / `call.resume` |
| `call.publish.answer` | `{call_id, sdp}` | reply to `call.publish` |
| `call.subscribe.offer` | `{call_id, sdp, version, streams: [SubStream]}` | whenever remote streams change; answer with `call.subscribe.answer` |
| `call.ice` | `{call_id, pc, candidate}` | SFU's trickled candidates |
| `call.participant` | `{call_id, event: "joined"\|"updated"\|"left", participant: Participant}` | roster change (excluding self) |
| `call.ok` | `{}` | generic success reply |
| `call.ended` | `{call_id, reason}` | call torn down under the participant (e.g. SFU restart) |
| `channel.call` | `{channel_id, call_id\|null, participant_count}` | sent to **all** channel members (in the call or not) so UIs can show "call in progress · join" |

```text
Participant = { participant_id, user_id, display_name,
                audio: bool, video: bool,             // mute state from call.media
                publishing: [ { kind: "audio"|"video", source: "mic"|"camera"|"screen" } ] }
SubStream   = { mid, participant_id, kind: "audio"|"video", source }
```

`SubStream.mid` is a mid **in the receiving client's own subscribe PC**. Mids differ
per receiver, which is why the mapping travels with each `call.subscribe.offer`
rather than inside `Participant`: it lets the client map an incoming track
(`transceiver.mid`) to its participant without parsing SDP. A mid absent from the
latest `streams` is inactive. `version` increases monotonically. The client answers
only the latest offer and echoes its `version`; the server rejects an answer whose
`version` is not the latest with `error: stale` (the client then answers the newer
offer it has, or will shortly receive). Each PC has one fixed offerer, so there
is no glare.

### 3.5 Sequences

**Join and publish**

```text
C → call.join {channel_id}
S → call.joined {call_id, self, participants}
C → call.publish {call_id, sdp: offer}          (publish PC, sendonly)
S → call.publish.answer {call_id, sdp: answer}
C ⇄ S  call.ice {pc: "publish"} …               (trickle, both directions)
S → call.subscribe.offer {call_id, sdp, version} (only if anyone else publishes)
C → call.subscribe.answer {call_id, sdp}
C ⇄ S  call.ice {pc: "subscribe"} …
```

**Someone else joins/publishes**: `S → call.participant {event:"joined"}`, then a new
`S → call.subscribe.offer` including their streams. **They leave**:
`S → call.participant {event:"left"}` and a new offer without their streams.

**WS drops mid-call.** Media keeps flowing (it doesn't use the WS). The server keeps
the participant for a **30 s grace**. The client reconnects, sends `auth`, then
`call.resume {call_id}`; the server replies `call.joined` (fresh roster) and re-sends
the latest `call.subscribe.offer` if it is still unanswered. The **publish PC is
untouched** by a WS reconnect (its media never used the WS), so there is no
publish renegotiation and no re-sent `call.publish.answer`. After the grace
the participant is removed as if it had sent `call.leave`; a later `call.resume`
gets `error: not_in_call` and the client must `call.join` again.

### 3.6 Limits (MVP)

- **8 participants** per call (`call_full` beyond).
- Per-publisher cap **1.5 Mbps video / 720p** set in the SFU room config, until
  simulcast lands ([MEDIA.md](MEDIA.md) §5).
- One camera + one mic per participant. Screen share is a later additive change
  (a third `source`), not in v1.
- ICE: host candidates suffice on the shared LAN test server; STUN/TURN delivery
  (`ice_servers` in `call.joined`) is added with coturn, as an **additive** field.

### 3.7 Implementation notes (server-internal, not contract)

`call.join` → Janus session + VideoRoom handle, `join` as `publisher` → the
`joined` event yields the publisher id and `private_id` (kept server-side).
`call.publish` → `configure` with the client's offer. The subscribe PC is a
second handle joined as `subscriber` with `streams:[{feed}]` and the
`private_id`; Janus's offer becomes `call.subscribe.offer`, the client's answer
goes to `start`, and later roster changes use `update`
(`subscribe`/`unsubscribe`). Janus event handlers (not WS close) are
authoritative for "participant gone" ([SECURITY.md](SECURITY.md) §7).

## 3a. Push & mobile background (decided)

Mobile OSes suspend background WebSockets, so an always-on WSS cannot be the delivery path on iOS/Android. We add **push**:

- **Device registration:** clients register an **APNs (iOS) / FCM (Android)** token via `POST /devices` (revoked on logout / `DELETE /devices/{id}`).
- **Wake on event:** push is decided **per device, not per user.** A user's desktop having a live WS must **not** suppress push to their mobile (they may have walked away). Each registered mobile device gets a push (silent data / collapse-key for messages; high-priority for call invites) unless *that device* has a live foreground socket. Optionally gate by per-device presence (active/idle/away). Desktop clients with an always-on WS simply don't need push.
- **Incoming calls:** delivered as high-priority push → **CallKit (iOS) / ConnectionService + foreground service (Android)** present the native incoming-call UI; the app joins the room on accept.
- **Privacy:** push payloads carry minimal metadata (e.g. "new message in #x"), not message content, unless the user opts into content previews.
- **Provider config** (APNs key/FCM credentials) is per-deployment operator config. Desktop builds omit push.

## 4. Slash commands & bots

- `slash.command` with text matching `^/(?<bot>\w+)\s+(?<msg>.*)$` → `api` looks up the bot, signs and POSTs `{channel, user, message}` to the bot's URL (SSRF-guarded).
- Bot replies arrive via the inbound webhook (`POST /bots/{id}/webhook`, HMAC-verified) and are broadcast as `bot.message`.

## 5. Conventions, limits & errors

- IDs are UUIDv7 (sortable). Timestamps ISO-8601 UTC.
- Idempotency: client supplies a client-side message id; server dedupes.
- Offline: `core` queues outgoing commands and replays on reconnect; server dedupes by client id.
- Versioning: REST is path-versioned under the **`/api/v1`** base (matching §1); WS envelope may carry a `v` field later.
- **File upload states:** `pending` (POST Policy issued; MinIO enforces size via `content-length-range`) → `committed` (`POST /files/{id}/commit` confirmed object exists + type ok). Only `committed` files may be attached to messages; uncommitted/orphaned objects are reaped by a sweep.
- **Pagination:** `limit` default 50, **max 100**; page backwards with `before=<message_id>`.
- **Sizes & lifetimes** (TTLs, message/file/payload caps, rate limits): single source of truth is [SECURITY.md](SECURITY.md) §7.
- **Errors:** uniform JSON body `{ "error": { "code": "<machine_code>", "message": "<human>", "details?": {} } }` with a sensible HTTP status. Codes are a stable taxonomy (e.g. `auth.invalid_credentials`, `auth.totp_required`, `authz.forbidden`, `not_found`, `rate_limited`, `validation.*`, `conflict`). WS errors use an `error` event with the same shape.
