# core: WS commands + call signaling (C1b)

> Status: **draft** — awaiting review · 2026-09-24
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

### 3.1 Command path in `ws.rs`

- `run_once` becomes a `tokio::select!` loop over (a) incoming frames, (b) an `mpsc::Receiver<Outgoing>`
  of commands, (c) a `watch::Receiver<()>` "token refreshed" signal.
- `Outgoing = { frame: serde_json::Value /* has id */, reply: Option<oneshot::Sender<Result<Reply>>> }`.
  `call.ice` has no reply (`None`).
- **Pending map** `id → oneshot::Sender`. An incoming frame with `re` completes its entry: a success
  type → `Ok(Reply{type, data})`; `error` → `Err(CommandError{code, message})`. A frame with an
  unknown `re` is logged and dropped. Frames without `re` go down the existing event path.
- **Timeout**: the caller awaits the oneshot with a 10 s timeout → `Error::Timeout`. The pending entry is
  removed on timeout (a late reply is then an unknown `re`, dropped).
- **Disconnect**: when a connection ends, every pending sender is completed with `Err(Disconnected)`.
  Commands submitted while disconnected wait in the mpsc until the next `ready` (bounded: capacity 64;
  a full queue → `Error::Busy`, never unbounded memory).
- **Ids**: UUIDv4 strings per command.
- **Ready gating**: nothing but `auth` is written before `ready`. `ready` is published as today
  (`ServerEvent::Ready`), and now also carries whether it is a **reconnect** (a second `ready` on a new
  socket) so the call layer can resume.

### 3.2 Re-auth

- `refresh_loop` already rotates tokens in the shared session. It additionally signals the watch.
- On the signal, an open, `ready` socket sends the **same auth frame** with the new token (and an `id`);
  the matching `ready` (with `re`) confirms it. No reconnect.
- If the socket is closed with 1008 (`auth_failed` | `auth_timeout` | `token_expired`), the existing
  reconnect loop runs; the session's live token is read on reconnect (unchanged Phase 1 behaviour).
- The token is never logged; frames are logged by `type` only, never by content (SDPs can carry
  host IPs; tokens appear in `auth`).

### 3.3 Call state machine (`call.rs`)

```rust
pub enum PcKind { Publish, Subscribe }
pub struct IceCandidate { pub candidate: String, pub sdp_mid: Option<String>, pub sdp_mline_index: Option<u32> }
pub struct SubStream { pub mid: String, pub participant_id: String, pub kind: MediaKind, pub source: MediaSource }

#[async_trait]
pub trait MediaEngine: Send + Sync {
    async fn create_publish_offer(&self) -> Result<String, EngineError>;          // callable again (renegotiation)
    async fn apply_publish_answer(&self, sdp: String) -> Result<(), EngineError>;
    async fn apply_subscribe_offer(&self, sdp: String, streams: Vec<SubStream>) -> Result<String, EngineError>;
    fn add_remote_candidate(&self, pc: PcKind, c: Option<IceCandidate>) -> Result<(), EngineError>;
    fn set_local_media(&self, audio: bool, video: bool) -> Result<(), EngineError>;
    async fn close(&self);
}

impl BrookClient { pub async fn join_call(&self, channel_id: &str, engine: Arc<dyn MediaEngine>, publish: bool)
                       -> Result<Arc<CallHandle>>; }
impl CallHandle {
    pub fn state(&self) -> watch::Receiver<CallState>;
    pub fn local_candidate(&self, pc: PcKind, c: Option<IceCandidate>);  // sync, non-blocking (engine threads)
    pub fn engine_failed(&self, message: String);                         // sync; ends the call
    pub async fn set_media(&self, audio: bool, video: bool) -> Result<()>;
    pub async fn republish(&self) -> Result<()>;                          // renegotiate publish PC
    pub async fn leave(&self) -> Result<()>;
}
pub struct CallState { pub status: CallStatus, pub call_id: Option<String>, pub self_participant: Option<String>,
                       pub participants: Vec<Participant> }
pub enum CallStatus { Joining, Connected, Reconnecting, Ended(EndReason) }
pub enum EndReason { Left, SfuRestart, Removed, Expired, EngineFailed(String), Server(String /*code*/) }
```

One **call task** per `CallHandle` owns all state and processes, in order: server call events routed to
it by `call_id`, handle commands, local candidates, and WS `ready`/drop notices. No locks across awaits.

- **Join**: `call.join{channel_id}` → `call.joined` (roster, `self.participant_id`, `resume_token`). If
  `publish`: `engine.create_publish_offer()` → `call.publish{sdp}` → `engine.apply_publish_answer`.
  Status `Connected` once `call.joined` arrived (media connectivity is the engine's to report to its UI).
- **Subscribe**: `call.subscribe.offer{version, sdp, streams}` — offers are applied **strictly one at a
  time**. While one is being applied, newer offers overwrite a single "next" slot (latest wins; older
  queued ones are dropped unapplied). After `apply_subscribe_offer` returns an answer for version *v*:
  if a newer offer is waiting, the answer is **not sent** (it would be `stale`); the next one is applied.
  Otherwise `call.subscribe.answer{version: v, sdp}`; an `error: stale` reply is ignored (a newer offer
  is already on its way).
- **ICE**: local candidates → `call.ice` immediately (no reply). Remote `call.ice` for a PC is buffered
  until that PC's remote description has been applied (publish: after `apply_publish_answer`;
  subscribe: after the first `apply_subscribe_offer`), then flushed in arrival order; later ones pass
  straight to the engine. `null` = end-of-candidates, forwarded as `None`.
- **Roster**: `call.participant{joined|updated|left}` updates `participants`; `call.joined` replaces it.
- **Mute**: `set_media` → `engine.set_local_media` first; only on success `call.media{audio,video}`.
- **Resume**: WS drop → `Reconnecting`. On the next `ready`: `call.resume{call_id, participant_id,
  resume_token}` → `call.joined` (fresh roster, **rotated** token stored) → `Connected`. The publish PC is
  untouched. `error: not_in_call` → `Ended(Expired)`; engine closed. The resume token is kept only in
  the call task's memory and never logged.
- **End**: `call.ended{reason}` → `Ended(SfuRestart|Removed)`; `leave()` → `call.leave` (reply awaited,
  errors ignored) → `Ended(Left)`; `engine_failed` → `call.leave` + `Ended(EngineFailed)`. Every end
  path calls `engine.close()` exactly once.
- **Errors**: a command error while `Joining` → `Ended(Server(code))`; `call_full`/`not_member` surface
  from `join_call` as `Err` before a handle exists.

### 3.4 Events

`ServerEvent::ChannelCall { channel_id, call_id: Option<String>, participant_count: u32 }` on the existing
broadcast (the `channel.call` snapshot after `ready`, then changes). Call-specific events are routed to
the owning call task, not broadcast.

## 4. Not doing

- UniFFI exposure of `CallHandle`/`MediaEngine` (next step, with the macOS engine — C2).
- Any media code; the GTK adapter (brook-linux's, over `gst-media`).
- STUN/TURN `ice_servers` (additive later), screen share (`source: screen`, later).
- Changing the Phase 1 event path or chat API.
- Server behaviour (PR #10 is the source of truth; divergences are reported, not patched here).

## 5. Failure modes

| Risk | Consequence | Mitigation |
|---|---|---|
| Reply lost (server bug, socket half-dead) | caller hangs | 10 s timeout; pending removed; `Disconnected` on socket end |
| Commands queued during a long outage | memory growth | bounded mpsc (64) → `Busy` |
| Subscribe renegotiation storm (many joins) | engine thrash, stale answers | one-at-a-time apply, single "next" slot, never send an answer already superseded |
| Remote ICE before remote description | engine rejects candidate | per-PC buffer until applied |
| Token leaks via logs | credential exposure | frames logged by `type` only; `Session`/token types keep redacting `Debug` |
| Re-auth races a reconnect | double auth / wrong token | re-auth only on an open, `ready` socket; reconnect path always reads the live token |
| Engine call blocks the call task | stalls all signaling | engine async methods are awaited on the call task by design (they are the protocol's critical path); sync methods must be non-blocking (trait doc) |
| `engine.close()` called twice / never | leaked PCs, double free in FFI later | single end path in the call task, tested |

## 6. Tests (in-process WS server + fake engine)

| Test | Mutation it must catch |
|---|---|
| command gets its reply by `re` while an unrelated event arrives first | match replies by order instead of `re` |
| reply never comes → `Timeout` in ≤10 s (test uses a shortened timeout) | no timeout |
| socket closed with a pending command → `Disconnected` | pending senders leaked |
| token refresh → `auth` frame with new token on the same socket, no reconnect | reconnect instead / old token |
| join → publish offer → answer applied; call.joined roster in state | skip apply_publish_answer |
| two subscribe offers v1, v2 arrive while v1 is applying → only v2's answer sent, with version 2 | send every answer |
| remote ICE before subscribe offer → delivered after apply, in order | forward immediately |
| WS drop + ready → `call.resume` with participant_id + latest token; rotated token used on the next resume | resend the first token |
| resume → `not_in_call` → `Ended(Expired)`, engine closed once | leave status Reconnecting |
| set_media: engine error → no `call.media` sent | send before engine success |
| `call.ended` / `leave` / `engine_failed` → `engine.close()` exactly once each | close twice / never |
| no frame except `auth` is written before `ready` | write commands immediately on connect |
| logs never contain the token or resume token (captured `tracing` output) | log frames verbatim |

## 7. Live verification

A fake engine cannot produce SDP that Janus accepts, so a core-only smoke test against the real SFU is
not meaningful. The first real call — macOS engine (C2) or the GTK adapter over `gst-media`, with the
server's browser harness as the other party — is C1b's live acceptance.
