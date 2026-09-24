# core: WS commands + call signaling (C1b)

> Status: **revised after review round 1** (Codex + Vibe, Heavy) · 2026-09-24
> Review dial: **Heavy** (tokens on the socket; shared by every client). Both external models, given evidence.
> Wire contract: [PROTOCOL.md](../../PROTOCOL.md) §2–§3 as in PR #10 @ 8a26965 (owned by the server side).
> Consumers: GTK/KDE (`clients/gst-media` adapter, Rust), macOS/iOS (UniFFI, a later step).
> Touches: `core/src/ws.rs`, new `core/src/call.rs`, `core/src/client.rs` (small), `core/src/lib.rs` exports.

## 1. Problem

Phase 1's `ws.rs` is receive-only: it authenticates, then fans server events out. Calls need the
client to **send commands and match each reply** (`id` → `re`, exactly one reply, 10 s timeout),
to **re-authenticate** on the open socket when the access token is refreshed (a reconnect mid-call
is survivable but costs a resume round trip every 15 minutes), and a **call state machine** that
drives a platform `MediaEngine` (GStreamer on Linux, libwebrtc on Apple) without knowing either.

## 2. Done means (observable)

1. `cargo test -p brook-core` runs the new tests against an **in-process WebSocket server**
   (tokio-tungstenite `accept_async` on 127.0.0.1) that speaks the §2–§3 wire, and a **fake engine**.
   Each test is seen failing under the mutation named in §6.
2. Existing Phase 1 behaviour is unchanged: all current core tests pass; GNOME/KDE compile without
   source changes (`ServerEvent` is `#[non_exhaustive]`; new variants only).
3. `cargo fmt --check`, `clippy -D warnings`, `cargo deny` (no new failures vs `main`).
4. Live proof is deferred to the first real call (§7): C1b is accepted when a macOS or GTK engine
   completes a call through it against the shared test server.

## 3. Design

### 3.0 Session lifecycle fixes (prerequisite, existing Phase 1 code)

The review found three defects in the inherited session code; C1b builds on a session with these fixed:

- **Session revision.** The shared session becomes `{ revision: u64, session: Option<Session> }`. Login,
  logout and every committed refresh bump `revision`. Everything bound to an identity (socket
  authentication, pending commands, calls) records the revision it started under.
- **Refresh compare-and-set on both paths.** `refresh_once` already compares the refresh token before
  committing a success; its 4xx path must do the same (today a stale failure after a new login erases
  the new session). It returns `Committed | Discarded | Failed`, and only `Committed` notifies.
- **Login/logout invalidate the socket.** A revision change to a different user (or to none) closes the
  current socket, fails its pending commands with `Disconnected`, and ends active calls
  (`Ended(Server("session_changed"))`); the reconnect loop then authenticates as the new user. A refresh
  of the *same* user keeps the socket and re-auths it (§3.2).

### 3.1 Transport: commands, replies, connection generations (`ws.rs`)

- `run_once` becomes a `select!` over incoming frames, an outgoing-command mpsc, and a session-revision
  watch. Each authenticated socket gets a **connection generation** (`u64`, +1 per new socket),
  published on an internal `watch<Conn>` = `{ generation, ready: bool }`. `ServerEvent::Ready` stays a
  **unit variant** (GNOME/KDE match it); the generation watch is the internal mechanism.
- **No queueing across disconnects.** A command submitted while no socket is `ready` fails immediately
  with `Disconnected` (the call layer decides what to redo after resume). A command carries the
  generation it was created for; the transport drops it (`Disconnected`) if that generation is no
  longer current. So nothing queued for socket A is ever written to socket B.
- **Correlation registered before write.** The pending entry `id → (expected reply type, oneshot)` is
  inserted, then the frame is written; a write failure removes it. Outcomes: `NotSent` (never written),
  `Unknown` (written, socket ended before a reply), `Replied(Ok|Err)`.
- **Timeouts start at write** (10 s). On expiry the transport removes the entry and completes it with
  `Timeout`; a late reply is an unknown `re` and is dropped.
- **Reply validation.** A reply whose `type` is not the expected success type for that command (or
  `error`) is a protocol violation → `Err(UnexpectedResponse)`; the frame is not otherwise applied.
- **Call event routing without a gap.** For `call.join` and `call.resume`, the command carries the call
  task's mailbox. When the transport receives the correlated `call.joined`, it installs
  `call_id → mailbox` **before reading the next frame**, then completes the reply. So a
  `call.subscribe.offer` sent back-to-back with `call.joined` is never dropped. Call events for an
  unknown `call_id` are logged (type only) and dropped.
- **Outbound size.** Frames larger than 64 KiB are refused locally (`Error::TooLarge`) rather than
  triggering the server's 1009 close. A 1009 close is not treated as an auth problem.

### 3.2 Re-auth and 1008 recovery

- A `Committed` same-user refresh → the open, ready socket sends the same `auth` frame (new token,
  with `id`); the correlated `ready{re}` confirms it. It is a reply, **not** a new generation: calls do
  not resume. Re-auth failure (timeout or error) closes the socket → normal reconnect.
- A refresh notification arriving before the socket is ready is **not lost**: the socket records the
  session revision it authenticated with; on becoming ready it compares against the current revision
  and re-auths if newer.
- **1008 close** (`auth_failed` | `auth_timeout` | `token_expired`) → the reconnect loop runs a
  **single-flight REST refresh** (shared with `refresh_loop`) before reconnecting; if refresh fails with
  4xx the session is cleared (revision bump → `LoggedOut`) and the loop idles. The session lock is never
  held across a network await; the token is read, then released.

### 3.3 Engine contract (`call.rs`)

```rust
#[non_exhaustive] pub enum MediaKind { Audio, Video, Unknown }          // wire: "audio" | "video"
#[non_exhaustive] pub enum MediaSource { Mic, Camera, Screen, Unknown }  // wire: lowercase
pub enum PcKind { Publish, Subscribe }                                   // wire: "publish" | "subscribe"
pub struct IceCandidate { pub candidate: String, pub sdp_mid: Option<String>, pub sdp_mline_index: Option<u32> }
                                                                         // wire: candidate, sdpMid, sdpMLineIndex
pub struct IceServer { pub urls: Vec<String>, pub username: Option<String>, pub credential: Option<String> }
pub struct SubStream { pub mid: String, pub participant_id: String, pub kind: MediaKind, pub source: MediaSource }

#[async_trait]
pub trait MediaEngine: Send + Sync {
    /// New sendonly offer for the publish PC, set as its local description. Callable again (renegotiation).
    async fn create_publish_offer(&self) -> Result<String, EngineError>;
    /// Sets the publish PC's remote description.
    async fn apply_publish_answer(&self, sdp: String) -> Result<(), EngineError>;
    /// Sets the subscribe PC's remote description to `sdp`, creates the answer AND sets it as the local
    /// description (a complete offer/answer transition), returns the answer. `streams` maps its mids.
    async fn apply_subscribe_offer(&self, sdp: String, streams: Vec<SubStream>) -> Result<String, EngineError>;
    /// MUST NOT block or do I/O (called on core's call task).
    fn add_remote_candidate(&self, pc: PcKind, c: Option<IceCandidate>) -> Result<(), EngineError>;
    /// MUST NOT block. Local mute/camera state.
    fn set_local_media(&self, audio: bool, video: bool) -> Result<(), EngineError>;
    /// MUST NOT block. Applies to PeerConnections created after the call; default: ignore.
    fn set_ice_servers(&self, _servers: Vec<IceServer>) {}
    /// Stops capture and tears down both PCs. Called exactly once.
    async fn close(&self);
}
```

The async operations must complete on local WebRTC work alone (description setting, answer creation,
bounded capture start), never waiting on ICE connectivity or on anything core delivers — confirmed for
the GStreamer engine; required of the libwebrtc engine.

### 3.4 Call task

One task per `CallHandle` owns all call state. It **never awaits an engine operation inline**: engine
futures are spawned; their completions come back to the task's mailbox tagged with the operation and
the call's incarnation. At most one description operation per PC is in flight. So the task always keeps
servicing: server events, handle commands, local candidates, connection changes, and completions.

**Join.** `join_call` requires a ready socket (else `Err(Disconnected)`), sends `call.join{channel_id}`
with the task's mailbox (routing per §3.1), and returns the handle once `call.joined` arrives
(`call_full`/`not_member`/… → `Err`). On `call.joined`: store `participant_id`, `resume_token`, roster;
`engine.set_ice_servers(...)` if present; status `Connected`. If `publish`: spawn `create_publish_offer`.

**Publish negotiation state** `Idle | Offering | AwaitingAnswer{sent_generation} | Stable`. Offer done →
`call.publish{call_id, sdp}` → answer → spawn `apply_publish_answer` → `Stable`. `republish()` from
`Stable` repeats it. A publish error reply or engine error while negotiating → the call ends
(`Ended(EngineFailed|Server)`): v1 does not try to recover a half-negotiated publish PC.

**Subscribe versions.** State: `latest_received` (version, sdp, streams), `applying` (version),
`answer_pending_ack` (version, answer), `acked` (version). Rules:
- An offer with version ≤ `acked` is ignored. An offer equal to `answer_pending_ack.version` (the
  server's replay after resume) → **resend the retained answer**, do not re-apply.
- Otherwise it becomes `latest_received`. When nothing is applying, the task spawns
  `apply_subscribe_offer(latest_received)`. On completion for version *v*: the stream mapping for *v*
  becomes current; if a newer offer arrived meanwhile, the answer for *v* is kept only locally and the
  newer offer is applied next (each offer is a complete description of the server's current state, so
  skipping superseded ones loses nothing); otherwise `call.subscribe.answer{call_id, version: v, sdp}`
  → `answer_pending_ack`; `call.ok` → `acked`. `error: stale` → drop the pending answer (a newer offer
  is already queued or arriving).

**ICE.**
- Outgoing: local candidates → `call.ice{call_id, pc, candidate}` only while `Connected` on the current
  generation; otherwise they are dropped (a new publish negotiation after recovery gathers new ones —
  v1 has no ICE restart, and the publish PC's ICE session survives a WS reconnect).
- Incoming, per PC: a candidate whose `sdp_mid` is not in the PC's **currently applied** description
  (publish: mids parsed from the applied answer's `a=mid:` lines; subscribe: the applied version's
  `streams`) is buffered, in order, until a description containing that mid is applied. `null`
  (end-of-candidates) is queued behind them. v1 relies on the contract stating **no ICE restarts**
  (raised with the server side); under that rule a candidate for an already-applied mid is always valid.

**Resume.** Generation change while in a call → `Reconnecting`. Once the new socket is ready: send
`call.resume{call_id, participant_id, resume_token}` (routing per §3.1). On `call.joined`: store the
**rotated** token, replace roster, `Connected`; then redo work the drop interrupted: a publish in
`Offering/AwaitingAnswer` → restart the publish negotiation (new offer); a pending subscribe answer →
resend it (the server re-sends the latest unanswered offer, handled by the version rules). No call
command other than `call.resume` is written on the new generation before this `call.joined`.
`not_in_call` → `Ended(Expired)`. A lost `call.joined` after the server rotated the token leaves core
with a spent token; the next resume then ends the call as `Expired` unless the server adopts the
one-step lookback proposed to it — a documented limit, not silent recovery.

**Mute.** `set_media`: `engine.set_local_media` first; on success `call.media{call_id, audio, video}`; if
that command fails, the engine state is rolled back to the previous values and the error returned.

**End.** Every terminal cause — `leave()`, `call.ended`, `engine_failed`, `not_in_call`, session change,
handle dropped — goes through one `finish(reason)`: set `Ended(reason)` (idempotent: first reason wins),
**immediately** spawn `engine.close()` (guarded by a flag, exactly once), remove the route, then (for
`leave` only) send `call.leave` best-effort without waiting to close media. After `Ended`, handle
methods return `Err(Ended)` and late engine completions or server events are ignored (incarnation check).

### 3.5 Events

`ServerEvent::ChannelCall { channel_id, call_id: Option<String>, participant_count: u32 }` on the broadcast.
Call-specific events go only to the owning call task (§3.1).

### 3.6 Logging

Core logs frames by `type` only, never payloads; tokens and resume tokens are held in a `Secret` newtype
whose `Debug`/`Display` redact. Dependency logging is outside core's control: tungstenite logs full
messages at `trace`, so every client's logging setup must cap `tungstenite`/`tokio_tungstenite` at `info`
(documented in core's README; the GNOME/KDE/macOS setups set it). The log test (§6) runs core with a
trace-level subscriber on core's targets and asserts neither token appears raw, hex- or base64-encoded.

## 4. Not doing

- UniFFI exposure of `CallHandle`/`MediaEngine` (next step, with the macOS engine — C2).
- Any media code; the GTK adapter (brook-linux's, over `gst-media`).
- STUN/TURN `ice_servers` (additive later), screen share (`source: screen`, later).
- Changing the Phase 1 event path or chat API.
- Server behaviour (PR #10 is the source of truth; divergences are reported, not patched here).

## 5. Failure modes

| Risk | Mitigation |
|---|---|
| Engine future stalls signaling | engine ops spawned; the task keeps servicing its mailbox; one description op per PC |
| Commands replayed on a new socket | no cross-disconnect queue; commands bound to a generation; only `call.resume` before `call.joined` |
| First subscribe offer lost to routing race | transport installs the route while processing `call.joined`, before the next frame |
| Superseded or replayed subscribe answers | explicit version states; retained answer for replay; ignore ≤ acked |
| Candidate for a mid not yet applied | per-PC buffer keyed by applied mids; contract "no ICE restart in v1" |
| Stale refresh erases a new login / socket keeps old identity | session revision; CAS on both refresh paths; revision change invalidates socket, pendings, calls |
| 1008 loops beyond the 30 s grace | single-flight REST refresh before reconnecting |
| Token / resume token in logs | `Secret` type; type-only frame logs; dependency log cap documented and set by clients |
| Double / missing `engine.close()` | one `finish()` path, flag-guarded, immediate |
| Lost resume reply after rotation | documented limit → `Expired`; lookback proposed to the server side |

## 6. Tests (in-process WS server + fake engine; each seen failing under its mutation)

| Test | Mutation |
|---|---|
| two concurrent commands, replies in reverse order → each gets its own | match by order |
| reply with the wrong success type → `UnexpectedResponse` | accept any `re` match |
| command while not ready → `Disconnected`, nothing written after reconnect | queue across disconnects |
| socket drops with a command in flight → `Unknown`; pending map empty | leak pending |
| timeout starts at write, not submit (submit during outage fails fast; slow reply → `Timeout`) | start at submit / no timeout |
| same-user refresh → `auth` with new token on the same socket, `ready{re}`, no resume | reconnect / resume on re-auth |
| refresh arrives before `ready` → re-auth right after ready | lose the notification |
| 1008 `token_expired` → REST refresh runs before the reconnect | reconnect with the old token |
| stale refresh failure after a new login leaves the new session intact | unconditional clear on 4xx |
| login as another user while a socket is open → socket closed, calls end `session_changed` | keep socket |
| `call.joined` and the first `call.subscribe.offer` sent back-to-back → offer applied | register route after completing reply |
| engine `apply_subscribe_offer` held at a barrier while v2 and v3 arrive and a local candidate is emitted → candidate still sent; only v3 answered, with version 3 | await engine inline / answer every version |
| resume replays the unanswered version → retained answer resent, engine not re-applied | re-apply / drop as duplicate |
| candidate for a new mid before the re-offer is applied → delivered after it, in order with `null` | pass straight through once any description exists |
| WS drop during publish `AwaitingAnswer` → after resume a new publish offer is sent | leave publish half-negotiated |
| nothing but `call.resume` is written on the new socket before its `call.joined` | drain queued commands on ready |
| resume token rotated twice → second resume uses the second token | reuse the first token |
| `leave` + `call.ended` + `engine_failed` racing while an engine op is pending → `close()` once, promptly | close per path / after reply |
| `set_media` where `call.media` fails → engine rolled back | leave engine muted |
| event for an unknown `call_id` → ignored | route to any call |
| unknown `source` ("screen2") deserializes to `Unknown` | strict enum |
| trace-level core logs contain no token / resume token (raw, hex, base64) across auth, re-auth, join, resume | log frames verbatim |

## 7. Live verification

A fake engine cannot produce SDP that Janus accepts, so a core-only smoke test against the real SFU is
not meaningful. The first real call — macOS engine (C2) or the GTK adapter over `gst-media`, with the
server's browser harness as the other party — is C1b's live acceptance.

## 8. Review log

**Round 1 — Codex + Vibe (Heavy).** Codex did not approve as written; all findings accepted: engine ops no
longer awaited inline (deadlock); explicit subscribe version states incl. replay after resume; ICE
buffered per applied mid (contract "no ICE restart" requested from the server side); no queueing across
disconnects, generation-bound commands, only `call.resume` before `call.joined`; timeouts from write;
reply-type validation; route installed while processing `call.joined` (lost-first-offer race);
single-flight refresh on 1008; three pre-existing session defects fixed as §3.0 (stale-failure erase,
`Ok(true)` on discarded refresh, login not invalidating the socket); `Ready` kept a unit variant;
per-operation failure policy with immediate `engine.close()`; `Secret` type and dependency log cap;
exact wire names. Vibe: timeouts from write, non-blocking sync methods, close-once flag, unknown-call_id
and delayed-sync tests — adopted; "drops intermediate offers" rebutted (each offer is a complete
description); "ICE during renegotiation" folded into the per-mid rule. From the GTK side: `set_ice_servers`
(default no-op) and `Unknown` enum variants added; engine async ops confirmed to complete on local work.
Contract questions sent to the server side: resume-token lookback, "no ICE restart in v1", `call.ice`
exemption wording, wire key names, replay keeps the same version.
