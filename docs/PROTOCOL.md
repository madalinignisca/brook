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
| `POST /channels/{id}/calls` | join call → `{room_id}` (signaling then over WS; api-proxied, no client Janus token) |
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
| `call.signal` | **relayed SDP/ICE** from SFU/peer (see §3) |
| `call.participant` | join/leave/mute in a room |

### Client → server commands
> **Sending messages is REST-only** (`POST /channels/{id}/messages`), never a WS command — one send path avoids races between HTTP retries and WS reconnect-replay, and simplifies dedup. The WS carries only ephemeral signals (typing, call) and **receives** fan-out.

| type | data |
|---|---|
| `auth` | access token (**required first frame**, see above) |
| `typing` | channel id |
| `call.join` / `call.leave` | room id |
| `call.signal` | SDP offer/answer, ICE candidates (to SFU) |
| `slash.command` | channel id, raw text (e.g. `/botname hello`) → triggers outbound webhook |

## 3. Call signaling (over the WS, relayed to Janus)

`core` runs the signaling state machine; the WS is just the transport between client and `api`, which proxies to the Janus VideoRoom API.

```
client.core ──call.join(room)──► api ──► Janus: join room
client.core ◄─call.signal(SDP/ICE)─ api ◄─ Janus negotiation
   ... DTLS/SRTP media flows CLIENT ↔ SFU directly (not via api) ...
```

The client's **media engine** produces the SDP/tracks and consumes remote tracks; `core` only shuttles signaling. See [MEDIA.md](MEDIA.md).

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
