# core — shared Rust client library

The **single shared brain** of every client. All non-UI logic lives here once; each native client is a thin UI over it. See [../docs/CLIENT_PHILOSOPHY.md](../docs/CLIENT_PHILOSOPHY.md).

## Responsibilities
- Auth/session + token refresh.
- REST client (control plane) + WebSocket client (realtime + call signaling relay), with reconnection/backoff and a **bounded** offline outgoing queue (cap ~100 commands; oldest-evict with a user-visible "failed to send" state — never unbounded memory growth).
- Local state model + event log; observable state for the UI.
- File transfer orchestration (presigned URL flow, progress, resume).
- **Call signaling** state machine (negotiates with Janus via the WS relay; drives the platform `MediaEngine`).
- Crypto: token handling, webhook/request signing. (No E2EE — non-goal; see [../docs/SECURITY.md](../docs/SECURITY.md).)
- Auth session lifecycle: secure token storage in the **platform keystore**, refresh, and driving the OIDC system-browser flow. See [../docs/AUTH.md](../docs/AUTH.md).
- Slash-command / bot parsing.

## Explicitly NOT here
UI, widgets, navigation, the file *picker* dialog, notifications, and the media *capture/encode/render* (that's the platform `MediaEngine` impl). `core` owns call **signaling**; the platform owns the **pixels and codecs**.

## Shape (planned)
- Async (Tokio). UI-agnostic API: observable state + command intents.
- `trait MediaEngine` — implemented per platform (GStreamer on Linux/Pi; native on others as needed). See [../docs/MEDIA.md](../docs/MEDIA.md).
- Bindings: **UniFFI** → Swift (macOS/iOS) & Kotlin (Android); **C ABI** → C#/.NET (Windows). On Linux/GNOME it's used directly (Rust↔Rust, no FFI).

## Why Rust
Fast, memory-lean, no GC pauses (matters on the Raspberry Pi), excellent FFI, compiles to all five targets.

## Calls (PROTOCOL.md §3)

```rust
client.start_realtime().await?;                       // the socket carries call signaling
let call = client.join_call(channel_id, engine, true).await?;   // engine: Arc<dyn MediaEngine>
let mut state = call.state();                          // watch::Receiver<CallState>
call.local_candidate(PcKind::Publish, Some(c));       // from the engine's own threads
call.set_media(false, true).await?;                    // mute (engine first, then server)
call.leave().await?;                                   // dropping the handle also leaves
```

Core owns signaling: the offer/answer state for both PeerConnections, subscribe versions,
ICE buffering per media section, resume after a socket drop, and exactly-once teardown.
The platform implements [`MediaEngine`](src/call_types.rs) — GStreamer on Linux
(`clients/gst-media`), libwebrtc on Apple — and must honour its contract: async
operations finish on local WebRTC work alone (never waiting for ICE or for core), the sync
methods never block, and `close()` fences every operation still running.

Design and tests: [docs/superpowers/specs/2026-09-24-core-call-signaling-design.md](../docs/superpowers/specs/2026-09-24-core-call-signaling-design.md).

## Logging: required cap for every client

Core never logs frame contents, tokens or resume tokens. **tungstenite does**: at `trace`
it logs whole WebSocket messages, including the `auth` frame's access token and
`call.joined`'s resume token. Every client that installs a tracing subscriber MUST cap
it, even when the user asks for `trace`:

```text
trace,tungstenite=info,tokio_tungstenite=info
```

`log_secrecy_tests` proves both halves: with this filter no token appears in any
encoding across auth, re-auth, join and resume; without the cap, tokens do leak.
