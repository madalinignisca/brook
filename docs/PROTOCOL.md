# Wire protocol

> Draft v0. Transport is split: **REST/HTTPS** for request/response, **WebSocket/WSS** for realtime + call signaling relay. All payloads JSON (a binary/CBOR option may come later for efficiency).

## 1. REST (control plane) — `https://<host>/api/v1`

| Method & path | Purpose |
|---|---|
| `GET  /auth/methods` | which methods this deployment enabled (local/oidc/ldap) |
| `POST /auth/login` | `{handle, password, supports_totp}` → `{access_token, refresh_token}`, or for a TOTP user `200 {totp_required, totp_token, expires_in}`; see §1.2 |
| `POST /auth/totp` | `{totp_token, code}` or `{totp_token, recovery_code}` → tokens (`+ recovery_codes_left` with a recovery code) |
| `POST /auth/totp/enroll` · `/activate` · `/disable` · `/recovery-codes` | set up, confirm, turn off, regenerate codes; see §1.2 |
| `POST /users/{id}/totp/reset` | **admin**: remove a member's TOTP `{admin_password}` → 204 |
| `GET  /auth/oidc/start` | begin OIDC (Auth Code + PKCE) in system browser |
| `GET  /auth/oidc/callback` | provider redirect; api exchanges provider code, issues a short-lived Brook code, redirects to the app |
| `POST /auth/oidc/exchange` | app exchanges the Brook code + PKCE verifier → `{access_token, refresh_token}` |
| `POST /auth/ldap` | LDAP bind credentials → tokens |
| `POST /auth/refresh` | refresh → new access token (rotates refresh token) |
| `POST /auth/logout` | revoke refresh token |
| `POST /auth/password` | change own password `{current_password, new_password}` → fresh `{access_token, refresh_token}`; see §1.1 |
| `GET  /users` · `?handle=` | **admin**: all users by handle · exact handle (404 `not_found` if none) |
| `POST /users/{id}/password` | **admin**: set a member's password `{admin_password, new_password}` → 204; see §1.1 |
| `GET  /health` | liveness/readiness (also on `sfu`; unauthenticated) |
| `GET  /me` · `PATCH /me` | current user · update profile/avatar |
| `GET  /channels` | channels/DMs the user belongs to |
| `POST /channels` | create channel |
| `GET /channels/{id}` · `PATCH /channels/{id}` · `DELETE /channels/{id}` | get / rename-topic / delete |
| `GET /channels/{id}/members` · `POST` · `DELETE /channels/{id}/members/{uid}` | list / add / remove member |
| `GET  /channels/{id}/messages?before=&after=&limit=` | history: `before=<id>` (back-paginate) or `after=<id>` (**forward-sync** missed messages on reconnect) |
| `POST /channels/{id}/messages` | **send a message (the only send path)**; server persists then fans out via WS. Optional `client_id` (UUID, per message): a resend with one the author already stored returns that message, **200** and unchanged (even with a different body), never a duplicate; `client_id` is echoed in the response and in `message.new` |
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

### 1.1 Password changes and sessions

- `POST /auth/password {current_password, new_password, sign_out_other_devices?}`
  needs a full access token and the current password. The response always carries
  a new pair for the calling client, plus `other_devices_signed_out: bool` saying
  what the server did. Word the confirmation from that field; if it is absent,
  the server predates the option (it revoked other devices' refresh tokens, but
  their access tokens lived out their 15 minutes).
- `sign_out_other_devices` (default **true**; the client shows it as a checkbox,
  checked by default) signs out **every other session at once**: all refresh
  tokens are revoked, every access token issued before the change is refused on
  REST (401 `auth.invalid_token`) and WebSocket, and open sockets are closed with
  `1008` / `session_revoked` (§2). With `false`, the password changes and other
  devices stay signed in.
- **With `sign_out_other_devices` (the default), the old refresh token is dead the
  moment the server commits.** A client that loses the response (timeout, dropped
  connection) still holds revoked tokens: its next `/auth/refresh` gets 401
  `auth.invalid_token`, it looks signed out, and signing in with the **new**
  password works. There is no idempotent retry. With `false`, nothing is revoked:
  the caller's old refresh token stays valid next to the new pair, and the client
  should keep the new one.
- The cut-off is millisecond-precise (`iat_ms` claim, compared with the user's
  `sessions_valid_after`), so a token minted earlier in the same second as the
  change is refused too. Tokens without `iat_ms` compare as `iat × 1000`, which
  can only make them look older.
- The **changing** device's own socket is closed as well (its token predates the
  change); its client already holds the new pair, so it reconnects and resumes any
  call (`call.resume`, §3).
- Wrong current password: **403** `auth.invalid_credentials`, deliberately not 401,
  so clients do not mistake it for an expired access token and refresh-and-retry.
  New password outside 8–256 characters, or equal to the current one: 422.
- `POST /users/{id}/password` (admin) always signs the target out everywhere, the
  same way (there is no opt-out: a reset is how a lost device is cut off). The admin re-authenticates with `admin_password` (wrong: 403
  `auth.invalid_credentials`), so a stolen admin access token alone cannot hand
  the thief lasting logins. It only works on **members**: the admin's own account
  is 400 `invalid`, another admin is 403 `authz.forbidden`. An admin password only
  ever changes through `POST /auth/password`. Non-admin caller: 403
  `authz.forbidden`; unknown id: 404 `not_found`.
- Token issue and revocation are serialised per user (the server locks the user
  row in login, refresh, password change and admin reset), so a refresh racing a
  password change cannot mint a token that outlives it.

### 1.2 TOTP (optional 2FA)

Full design: `docs/superpowers/specs/2026-09-25-totp-server-design.md`. The client
contract:

- **Login** sends `supports_totp: true`. For a TOTP user a correct password answers
  `200 {totp_required: true, totp_token, expires_in: 300}`. A client that doesn't send
  the flag gets `403 auth.totp_client_required` for such a user, never a 200 it would
  misread.
- **`POST /auth/totp`** completes it. It is never 401:
  - `403 auth.invalid_code` for a wrong, replayed or used code (a failed code does
    **not** burn the `totp_token`);
  - `403 auth.totp_expired` when the token is bad, expired or used, or the password
    changed or the user signed out everywhere since it was issued: restart at the
    password;
  - `429` + `Retry-After` when paced.
- **A code is accepted once, everywhere.** The code used to activate can't log in; the
  first login after activation needs the next code (≤ 30 s).
- **Enrolment** (full access session): `enroll {password}` → `{otpauth_uri,
  expires_in: 600}`, returned once (the client renders the QR);
  `activate {code}` → `{recovery_codes, access_token, refresh_token}`. Activation
  **signs out every other session**; commit its pair like `/auth/password`'s.
  `409 auth.totp_enrollment_expired` means scan again; `409 conflict` means TOTP is
  already on, or nothing is pending.
- **`disable`** and **`recovery-codes`** take `{password, code}`, where `code` may be a
  TOTP code or a recovery code (`recovery_code` as its own field works too).
  Regenerating replaces every old code.
- **`GET /auth/me`** adds `totp_enabled` and `recovery_codes_left` (null when off).
- **Recovery codes** look like `iiii-xxxx-xxxx-xxxx-xxxx`; case, dashes and spaces
  are forgiven.

## 2. WebSocket (realtime plane) — `wss://<host>/ws`

**Authentication** (matches `core/src/ws.rs` and `services/api/app/routers/ws.py`):

- The first frame must be `{"type":"auth","data":{"access_token":"…"}}`, sent within
  **5 s** of opening (SECURITY.md §7), never as a query parameter (query strings leak
  into logs/proxies). The server answers `{"type":"ready","data":{"user_id":…}}`.
- Any auth failure (missing/garbled frame, bad or non-access token, timeout)
  **closes with `1008`**; the close *reason* says which: `auth_failed`,
  `auth_timeout`. The client's remedy is the same for all: refresh over REST, then
  reconnect.
- `1008` / `rate_limited`: the server refused to examine the token because this
  client IP has been failing authentication (REST logins, refreshes and WS `auth`
  frames share one budget). The token may still be valid. Wait; do not refresh in a
  loop, since `/auth/refresh` answers the same condition with `429` +
  `Retry-After`. Then reconnect.
- `1008` / `session_revoked`: the user signed out everywhere (a password change
  with `sign_out_other_devices`, or an admin reset; §1.1) after this token was
  issued. Sent to open sockets at once, and to a reconnect with such a token. Not
  counted as a failed login. The remedy is the usual refresh: on another device it
  fails (refresh tokens are revoked) and the app signs out; on the changing device
  it succeeds with the new pair. Clients that don't know this reason already treat
  it as a generic `1008` auth close, which is exactly this behaviour.
- The socket also closes with `1008` / `token_expired` when its access token
  expires. To avoid that, the client may send the same `auth` frame **again on the
  open socket** with a fresh token for the same user; the server answers another
  `ready` (with `re`, see below) and extends the socket's life. A token for a
  different user closes the socket.
- Right after `ready`, the server sends one `channel.call` per call already in
  progress in the user's channels (§3.4), so a client that just (re)connected sees
  them.
- Client → server frames are capped at **64 KiB** (close `1009`); server → client
  frames are not (an 8-participant subscribe offer is ~15 KiB).

**Commands and replies.** A client → server frame other than `auth` is a
**command**: it carries a fresh `id`, and the server sends **exactly one** direct
reply carrying `re: <that id>` (the one exception is `call.ice`, which has no
reply): its success frame, or
`{"type":"error","re":…,"data":{"code":…,"message":…}}` (codes in §3.2). An
unknown `type` or a malformed frame gets `error` with code `invalid`; the socket
stays open. `auth` may carry an `id` too, in which case its `ready` carries `re`.
Unsolicited events never carry `re`.

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
| `auth` | `{access_token}`: **required first frame**; may be re-sent to refresh (see above) |
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
  checked by `api` on every call command; a member removed from the channel is
  removed from its call (`call.ended {reason: "removed"}`). One user may join the
  same call from several devices; each device is its own participant, and one socket holds at most one participant per call. An archived channel is read-only: `call.join` gets `bad_state`.
- **Two PeerConnections per participant**, both terminated by the SFU:
  - **publish** — `sendonly`: the participant's mic, camera, and later screen.
    The **client offers**, the server answers. The client may **renegotiate** the
    same PC at any time (camera turned on later, a track added) by sending
    `call.publish` again with a new offer; it gets a new `call.publish.answer`.
    Publishing is optional: a listen-only participant never sends `call.publish`.
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
| `bad_state` | command out of order (e.g. `call.publish` before `call.joined`), this socket is already in that call, or the channel is archived |
| `stale` | `call.subscribe.answer` for a `version` that is no longer the latest; discard it and answer the newer offer |
| `sfu_unavailable` | Janus unreachable or refused; retry later |

### 3.3 Client → server commands

| type | data | reply |
|---|---|---|
| `call.join` | `{channel_id}` | `call.joined` |
| `call.publish` | `{call_id, sdp, tracks?}` (publish-PC **offer**; `tracks` labels its m-lines, see below) | `call.publish.answer` |
| `call.subscribe.answer` | `{call_id, version, sdp}` (subscribe-PC **answer** to the offer with that `version`) | `call.ok` or `error: stale` |
| `call.ice` | `{call_id, pc: "publish"\|"subscribe", candidate}` | none (fire-and-forget) |
| `call.media` | `{call_id, audio: bool, video: bool}` (mute state the user wants; the server announces each as *wanted AND published*, so an unpublished kind stays `false`, and a mute sent before the publish completes is kept when it does) | `call.ok` |
| `call.leave` | `{call_id}` | `call.ok` |
| `call.resume` | `{call_id, participant_id, resume_token}` (after a WS reconnect, §3.5) | `call.joined` |

**Track labels and screen share.** `tracks` is an optional list of
`{mid, kind: "audio"|"video", source}` labelling the offer's m-lines:

- Absent: every audio m-line is `mic` and every video m-line is `camera` (clients
  predating screen share keep working).
- Present: every **active** audio/video m-line must be labelled; inactive or
  rejected ones *may* be. Each label names an audio/video m-line of this offer,
  once, with a `source` valid for its kind (`audio`: `mic`; `video`: `camera` or
  `screen`). At most one **active** `screen`. A mid that stays active keeps its
  source: relabelling a live m-line is refused (stop it and share again).
  Anything else is `invalid`.
- An m-line is **active** unless its direction is `inactive`/`recvonly`, or its
  port is 0 **without** `a=bundle-only` (port 0 **with** `a=bundle-only` is live,
  per RFC 8843, and is how max-bundle clients such as webrtcbin write it).
- **Start sharing:** add a `sendonly` video m-line to the **same** publish PC,
  label it `screen`, and send `call.publish` again. **Stop sharing:** set that
  m-line's direction to `inactive` and publish again. To share again, re-enable
  that m-line or add a new one.
- **Never `stop()` a publish transceiver.** That rejects its m-line, and the
  browser may later **recycle** the slot under a new mid, which the SFU (Janus
  1.4.2) answers with the stale mid, breaking the PC. The server refuses any
  offer that changes the mid of an existing m-line position with `invalid`.
- Peers get a new stream through a normal `call.subscribe.offer` whose
  `SubStream.source` is `"screen"`, and see it in the sharer's
  `Participant.publishing`.
- `call.media` stays mic/camera only: its `video` flag is the camera. Sharing is
  on/off by publishing, never by `call.media`.

`candidate` is `{candidate, sdpMid, sdpMLineIndex}` (exactly these keys, as WebRTC's
`RTCIceCandidate.toJSON()` produces), or `null` for end-of-candidates; `pc` is the
lowercase string `"publish"` or `"subscribe"`. **No ICE restarts in v1:** every
`call.ice` belongs to the current ICE session of that PC, and candidates are
relayed in order per PC, so a client buffers by `mid` (until a description
containing that mid is applied) and never needs to reason about ICE generations.

**Who buffers ICE.** The server relays client candidates to the SFU immediately;
the SFU accepts them before or after the SDP. The **client** buffers any
server → client `call.ice` that arrives before it has applied the matching remote
description, and applies them after. (The SFU runs ICE-lite and normally puts
its candidates in the SDP, so server → client trickle is rare but allowed.)

### 3.4 Server → client events

| type | data | when |
|---|---|---|
| `call.joined` | `{call_id, channel_id, self: {participant_id, resume_token}, participants: [Participant]}` | reply to `call.join` / `call.resume` |
| `call.publish.answer` | `{call_id, sdp}` | reply to `call.publish` |
| `call.subscribe.offer` | `{call_id, sdp, version, streams: [SubStream]}` | any time after `call.joined`, independent of your own publishing, whenever the set of remote streams changes; answer with `call.subscribe.answer` |
| `call.ice` | `{call_id, pc, candidate}` | SFU's trickled candidates |
| `call.participant` | `{call_id, event: "joined"\|"updated"\|"left", participant: Participant}` | roster change (excluding self) |
| `call.ok` | `{}` | generic success reply |
| `call.ended` | `{call_id, reason: "sfu_restart"\|"removed"\|"replaced"}` | your participation ended without `call.leave`: the SFU restarted, you were removed from the channel, or another socket of yours took this participant over with `call.resume`. Later commands for that call get `not_in_call` |
| `channel.call` | `{channel_id, call_id\|null, participant_count}` | sent to **all** channel members (in the call or not) so UIs can show "call in progress · join" |

```text
Participant = { participant_id, user_id, display_name,
                audio: bool, video: bool,             // mic / camera: wanted (last call.media,
                                                      // default on) AND published
                publishing: [ { kind: "audio"|"video", source: "mic"|"camera"|"screen" } ] }
SubStream   = { mid, participant_id, kind: "audio"|"video", source }
```

`SubStream.mid` is a mid **in the receiving client's own subscribe PC**. Mids differ
per receiver, which is why the mapping travels with each `call.subscribe.offer`
rather than inside `Participant`: it lets the client map an incoming track
(`transceiver.mid`) to its participant without parsing SDP. A mid absent from the
latest `streams` is inactive. `version` is **per participant**: it starts at 1 for the first offer after
`call.join`, increases by one per offer, and continues across `call.resume` (a new
`call.join` is a new participant and starts again at 1). The client answers
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
C → call.subscribe.answer {call_id, version, sdp}
C ⇄ S  call.ice {pc: "subscribe"} …
```

**Someone else joins/publishes**: `S → call.participant {event:"joined"}`, then a new
`S → call.subscribe.offer` including their streams. **They leave**:
`S → call.participant {event:"left"}` and a new offer without their streams.

**WS drops mid-call.** Media keeps flowing (it doesn't use the WS). The server keeps
the participant for a **30 s grace**. The client reconnects, sends `auth`, then
`call.resume {call_id, participant_id, resume_token}` using the values from the
last `call.joined`; the server replies `call.joined` (fresh roster and a **new**
`resume_token`) and re-sends the latest `call.subscribe.offer` if it is still
unanswered: the **same frame, same `version`**, so a client that already answered
it may resend its retained answer.
The server accepts the new `resume_token` **and the one the client last used**
(one step of lookback): if the `call.joined` carrying a new token is lost to a
second drop, the client still resumes with the token it holds. The **publish PC is
untouched** by a WS reconnect (its media never used the WS), so there is no
publish renegotiation and no re-sent `call.publish.answer`. After the grace
the participant is removed as if it had sent `call.leave`; a later `call.resume`
gets `error: not_in_call` and the client must `call.join` again. The
`resume_token` is what proves this device *is* that participant: `user_id` alone
would let a second device of the same user take the first one's place. A wrong
token, participant or user is always the same `not_in_call`.

### 3.6 Limits (MVP)

- **8 participants** per call (`call_full` beyond).
- Per-publisher cap **1.5 Mbps video / 720p** set in the SFU room config, until
  simulcast lands ([MEDIA.md](MEDIA.md) §5).
- One mic, one camera and at most one screen per participant. The per-publisher
  bitrate cap applies; send the screen at a low frame rate (MEDIA.md).
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
