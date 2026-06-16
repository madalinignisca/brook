# Android client — Kotlin + Jetpack Compose (Material 3)

Native Android app over the shared Rust [`core`](../../core).

## Stack
- **Jetpack Compose** with **Material 3**, Kotlin.
- `core` via **UniFFI**-generated Kotlin bindings (JNI under the hood).
- Media: platform WebRTC + **MediaCodec** (HW H.264). See [../../docs/MEDIA.md](../../docs/MEDIA.md).

## Background & push (required — not optional on mobile)
Android suspends background WebSockets, so always-on WSS is not the delivery path. The app registers an **FCM** token (`POST /devices`) and relies on push to wake-and-sync messages and to present **incoming calls** (high-priority FCM → ConnectionService + foreground service). See [../../docs/PROTOCOL.md](../../docs/PROTOCOL.md) §3a.

## Native UX commitments (Material 3)
- Dynamic color / Material You, predictive back, themed app icon.
- Storage Access Framework for the file picker; system notifications; **foreground service for active calls**.
- **App Links** for OIDC redirect (custom scheme only as fallback).

## Packaging
Play Store (AAB).
