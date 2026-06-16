# Client philosophy: native per OS, shared core underneath

## The principle

Most cross-platform chat apps (Slack, Discord, Teams) ship **one web UI** wrapped in Electron on every OS. The result is an app that looks identical everywhere and feels native nowhere — heavy on RAM, indifferent to platform conventions and hardware.

smartChat takes the **opposite** stance, modelled on the Apple-ecosystem instinct that software should *respect the platform it runs on*:

> **The UI is 100% native to each OS. The logic is shared once. The app should feel like the OS built it.**

A macOS user gets real AppKit/SwiftUI behaviours; a GNOME user gets libadwaita and the system file picker; an Android user gets Material 3. We are explicitly **not** a brand with a signature look — we are the option that disappears into each platform and uses its hardware to the fullest.

## How we avoid writing the app five times: the shared Rust `core`

Going fully native per platform would normally mean re-implementing networking, the protocol, state management, file transfer, call signaling, and crypto **five times** — five sets of bugs. We refuse that.

Instead, **all non-UI logic lives once in [`core/`](../core/) (Rust)** and is compiled into every client. Each client is a **thin native UI** that:
1. observes state exposed by `core`,
2. sends user intents (commands) to `core`,
3. supplies platform capabilities back to `core` (file picker results, notification hooks, the platform's screen-capture/media engine).

```
        ┌───────────────────────────────────────────────────────────┐
        │  native UI (per platform — the ONLY thing rewritten)        │
        │  GTK4/libadwaita · SwiftUI/AppKit · WinUI3 · Compose · UIKit │
        └───────────────▲───────────────────────────┬────────────────┘
            observe state│                           │commands / platform caps
        ┌───────────────┴───────────────────────────▼────────────────┐
        │                    core/  (Rust, shared)                     │
        │  networking (WSS/HTTPS) · protocol · state · file transfer   │
        │  call signaling · crypto · bot client · reconnection logic   │
        └──────────────────────────────────────────────────────────────┘
```

This is a proven pattern (Signal, Matrix's `matrix-rust-sdk`, 1Password): native UI, shared Rust engine. Rust is chosen for the core because it is fast, memory-lean (no GC pauses — matters on a Pi), has excellent FFI, and compiles to every target.

> **FFI state-observation pattern (decide early).** "Observe state" across UniFFI is **not** automatic — UniFFI does not bridge a Rust async stream (e.g. a Tokio `broadcast`) to a Swift `@Published` / Kotlin `Flow`. The contract: **`core` exposes a callback/listener interface** (UniFFI callback interface) that the native layer implements and registers; the native side wraps those callbacks into its own observable (`@Observable`/`StateFlow`/etc.). On Linux (Rust↔Rust, GTK) the core's stream is consumed directly. Define this listener shape in the `core` API up front — it shapes every client.

## Per-platform stack

| Platform | UI toolkit / language | Core binding | HIG target |
|---|---|---|---|
| **Linux / GNOME** *(+ Raspberry Pi)* | **GTK4 + libadwaita**, Rust | direct (both Rust — no FFI) | GNOME HIG; follows system light/dark + accent via `AdwStyleManager` |
| **macOS** | **SwiftUI + AppKit**, Swift | **UniFFI** (Rust→Swift) | macOS HIG; menu bar, native windowing |
| **Windows** | **WinUI 3 (Windows App SDK)**, C#/.NET | C ABI (`csbindgen`/P-Invoke) | Fluent design; Mica, native title bar |
| **Android** | **Jetpack Compose (Material 3)**, Kotlin | **UniFFI** (Rust→Kotlin/JNI) | Material 3; predictive back, themed icons |
| **iOS** | **SwiftUI + UIKit**, Swift | **UniFFI** (Rust→Swift) | iOS HIG; shares much with macOS client |

> The **GNOME client is the reference client** and the **Raspberry Pi 4B target** (see [MEDIA.md](MEDIA.md)). On Linux the whole stack is Rust, so it's the simplest to build first and the place we prove the core API.

## What stays native (never pushed into `core`)

- All widgets/layout/navigation and platform UX conventions.
- The **file picker** — each platform's real one (on GNOME via `gtk::FileDialog` → xdg-desktop-portal; on macOS `NSOpenPanel`; etc.). `core` only receives the chosen path/stream.
- **Notifications**, dock/taskbar/menu-bar integration, system tray.
- The **media engine** capture/encode/render — platform hardware-accelerated (see [MEDIA.md](MEDIA.md)). `core` owns call *signaling*; the platform owns the *pixels and codecs*.

## What lives in `core` (shared, never duplicated)

- Auth/session, token refresh.
- REST client + WebSocket client (reconnection, backoff, presence).
- Local state model + event log; offline queue.
- File transfer orchestration (request presigned URL, drive upload/download, resume).
- **Call signaling** state machine (negotiate with Janus via the WS relay; produce/consume SDP/ICE).
- Crypto: token handling, request signing (no E2EE — non-goal).
- Auth session lifecycle (platform keystore for tokens; drives the OIDC system-browser flow).
- Bot/slash-command parsing and dispatch.

## Consequence for optimization

Because the heavy logic is one tight Rust library and the UI is native (no web engine), each client is small and fast. This is what makes the **Raspberry Pi 4B call** feasible — there is no Electron tax, and the media path uses the Pi's hardware H.264 directly.
