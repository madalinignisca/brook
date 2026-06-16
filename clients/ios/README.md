# iOS client — Swift + SwiftUI / UIKit

Native iOS app over the shared Rust [`core`](../../core). Shares a large portion of its Swift layer with the macOS client.

## Stack
- **SwiftUI** (UIKit where needed).
- `core` via **UniFFI**-generated Swift bindings.
- Media: platform WebRTC + **VideoToolbox** (HW H.264/HEVC). GStreamer is likely **not** used here (App Store footprint/complexity) — the `MediaEngine` trait makes this swap clean. See [../../docs/MEDIA.md](../../docs/MEDIA.md).

## Background & push (required — not optional on mobile)
iOS suspends background WebSockets, so always-on WSS is not the delivery path. The app registers an **APNs** token (`POST /devices`) and relies on push to wake-and-sync messages and to present **incoming calls via CallKit**. See [../../docs/PROTOCOL.md](../../docs/PROTOCOL.md) §3a.

## Native UX commitments (iOS HIG)
- Native navigation, share sheet, document picker, dynamic type, system appearance.
- **CallKit** incoming-call UI; **Universal Links** for OIDC redirect (custom scheme only as fallback).

## Packaging
App Store.
