# core: WS commands + call signaling (C1b)

> Status: **approved** (Heavy review rounds 1–2 closed; round-2 findings applied, see §8) · 2026-09-24
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

- **Session epoch vs credential revision.** The shared session becomes
  `{ epoch: u64, credential_rev: u64, session: Option<Session> }`. `epoch` changes on every login, logout
  or session clear (identity changes — even logout→login as the same user, which a coalescing watch
  could otherwise hide: the epoch is compared, not the user id). `credential_rev` changes on every
  committed refresh within an epoch. Sockets, pending commands and calls are bound to the **epoch**; a
  credential change only triggers re-auth (§3.2).
- **Refresh compare-and-set on both paths.** `refresh_once` already compares the refresh token before
  committing a success; its 4xx path must do the same (today a stale failure after a new login erases
  the new session). It returns `Committed | Discarded | Failed`. Watchers are notified on every
  *committed* change: a committed refresh (credential_rev) and a committed clear after a matching 4xx
  (epoch). A discarded result notifies nothing.
- **Login/logout invalidate the socket.** An epoch change closes the
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
- **Call event routing without a gap, in order.** For `call.join` and `call.resume`, the command carries
  the call task's mailbox. When the transport receives the correlated `call.joined`, it installs
  `call_id → mailbox` **and delivers the `call.joined` itself into that mailbox**, both before reading
  the next frame. The call task therefore always processes `call.joined` (and installs joined/resumed
  state) before any event that followed it. Call events for an unknown `call_id` are logged (type only)
  and dropped.
- **Abandoned join.** If the `join_call` future is dropped: a not-yet-written `call.join` is skipped; if it
  was written, the transport, on receiving its `call.joined` with no live receiver, immediately sends
  `call.leave{call_id}` so no orphaned participant remains.
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
    /// Stops capture and tears down both PCs. Called exactly once. **Fences** the engine: every operation
    /// still running must, when it completes, leave capture stopped and the PCs closed (a capture start
    /// that finishes after close is torn down immediately), and its result is an error.
    async fn close(&self);
}
```

The async operations must complete on local WebRTC work alone (description setting, answer creation,
bounded capture start), never waiting on ICE connectivity or on anything core delivers — confirmed for
the GStreamer engine; required of the libwebrtc engine.

### 3.4 Call task

One task per `CallHandle` owns all call state. It **never awaits an engine operation inline**: engine
futures are spawned; their completions come back to the task's mailbox tagged with the call's
incarnation and a per-PC **operation sequence number**. A completion whose sequence number is not the
PC's current one is ignored. At most one description operation per PC is in flight; a new one is only
started after the previous completed. Completions never write to the socket directly: they update state,
and writes happen only through the state rules below, on the current generation. So the task always keeps
servicing: server events, handle commands, local candidates, connection changes, and completions.

**Join.** `join_call` requires a ready socket (else `Err(Disconnected)`), sends `call.join{channel_id}`
with the task's mailbox (routing per §3.1), and returns the handle once `call.joined` arrives
(`call_full`/`not_member`/… → `Err`). On `call.joined`: store `participant_id`, `resume_token`, roster;
`engine.set_ice_servers(...)` if present; status `Connected`. If `publish`: spawn `create_publish_offer`.

**Publish negotiation state** `Idle | Offering(seq) | AwaitingAnswer(seq) | ApplyingAnswer(seq) | Stable`.
Offer done → `call.publish{call_id, sdp}` → answer → spawn `apply_publish_answer` (`ApplyingAnswer`) →
completion → `Stable`. `republish()` only from `Stable` (else `Err(Busy)`). Any failure of the publish
negotiation — error reply, `Timeout`, `UnexpectedResponse`, engine error — ends the call
(`Ended(Server(code))` / `Ended(EngineFailed)`): v1 does not recover a half-negotiated publish PC.

**Subscribe versions.** State: `latest_received` (version, sdp, streams), `applying` (version),
`answer_pending_ack` (version, answer), `acked` (version). Rules:
- An offer with version ≤ `acked` is ignored. An offer equal to `answer_pending_ack.version` (the
  server's replay after resume) → **resend the retained answer**, do not re-apply.
- Otherwise it becomes `latest_received`. When nothing is applying, the task spawns
  `apply_subscribe_offer(latest_received)`. On completion for version *v*: the stream mapping for *v*
  becomes current; if a newer offer arrived meanwhile, the answer for *v* is kept only locally and the
  newer offer is applied next (each offer is a complete description of the server's current state, so
  skipping superseded ones loses nothing); otherwise `call.subscribe.answer{call_id, version: v, sdp}`
  → `answer_pending_ack` (exactly one retained answer: the latest sent; older ones are discarded, so
  memory is bounded). Every reply is matched to the **version it was sent for**: `call.ok` for *v*
  sets `acked = max(acked, v)` (monotonic) and clears `answer_pending_ack` only if it still holds *v*;
  `error: stale` for *v* clears only a pending answer that is still *v*. A subscribe-answer `Timeout` /
  `Unknown` keeps the answer retained (the server replays the offer after resume or re-offers); any
  other error code, or an engine error applying an offer, ends the call.

**ICE.**
- Outgoing: local candidates → `call.ice{call_id, pc, candidate}` only while `Connected` on the current
  generation; otherwise they are dropped (a new publish negotiation after recovery gathers new ones —
  v1 has no ICE restart, and the publish PC's ICE session survives a WS reconnect).
- Incoming, per PC: a candidate whose `sdp_mid` is not in the PC's **currently applied** description
  (publish: mids parsed from the applied answer's `a=mid:` lines; subscribe: the applied version's
  `streams`) is buffered, in order, until a description containing that mid is applied. `null`
  (end-of-candidates) is queued behind them. A candidate with `sdp_mid: null` is resolved by
  `sdp_mline_index` against the applied description's m-lines; with neither resolvable it is buffered
  the same way. v1 relies on the contract stating **no ICE restarts**
  (raised with the server side); under that rule a candidate for an already-applied mid is always valid.

**Resume.** Generation change while in a call → `Reconnecting`. Once the new socket is ready: send
`call.resume{call_id, participant_id, resume_token}` (routing per §3.1). On `call.joined`: store the
**rotated** token, replace roster, `Connected`; then redo work the drop interrupted: a publish in
`AwaitingAnswer` (offer sent, answer lost with the socket) → restart the publish negotiation with a new
offer; `Offering`/`ApplyingAnswer` (an engine op in flight) → let it complete first, then continue from
the resulting state (an offer completed in `Offering` is sent on the new generation); a pending
subscribe answer → resend it (the server re-sends the latest unanswered offer, handled by the version rules). No call
command other than `call.resume` is written on the new generation before this `call.joined`.
`not_in_call` → `Ended(Expired)`. A lost `call.joined` after the server rotated the token leaves core
with a spent token; the next resume then ends the call as `Expired` unless the server adopts the
one-step lookback proposed to it — a documented limit, not silent recovery.

**Mute.** `set_media` calls are serialized (one `call.media` in flight; a newer intent replaces a queued
one). `engine.set_local_media` first; on success `call.media{call_id, audio, video}`. An **error reply**
(server rejected) rolls the engine back — only if no newer intent has been applied since — and returns the
error. A `Timeout`/`Unknown` outcome does **not** roll back (the server may have applied it): the intent
is kept and re-sent after resume, so engine and server converge on the latest local intent.

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

**Round 2 — Codex + Vibe (Heavy), final.** Vibe: all resolved; new: unbounded retained answers → exactly one
retained. Codex: still open 3/4/5/9/12 and five new P1s — none disputed, all applied without a round 3
(the convergence rule stops review rounds; it does not block accepting agreed fixes): index-only
candidates resolved by m-line; completions tagged with per-PC sequence numbers and never writing
directly; abandoned joins skipped or left; `call.joined` delivered through the mailbox for ordered
initialization; full publish/subscribe failure policy; `close()` fences outstanding engine operations;
publish `ApplyingAnswer` state and in-flight-aware resume; session **epoch** (identity) split from
**credential revision** (rotation); serialized mute with rollback only on rejection; version-guarded,
monotonic subscribe acknowledgements. "No ICE restart in v1" stays dependent on the server side
confirming it in the contract.
