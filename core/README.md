# core — shared Rust client library

The **single shared brain** of every client. All non-UI logic lives here once; each native client is a thin UI over it. See [../docs/CLIENT_PHILOSOPHY.md](../docs/CLIENT_PHILOSOPHY.md).

## Responsibilities
- Auth/session + token refresh.
- REST client (control plane) + WebSocket client (realtime + call signaling relay), with reconnection/backoff and an offline outgoing queue.
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
