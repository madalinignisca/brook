# iOS client — Swift + SwiftUI / UIKit

Native iOS app over the shared Rust [`core`](../../core). Shares a large portion of its Swift layer with the macOS client.

## Stack
- **SwiftUI** (UIKit where needed).
- `core` via **UniFFI**-generated Swift bindings.
- Media: platform WebRTC + **VideoToolbox** (HW H.264/HEVC). GStreamer is likely **not** used here (App Store footprint/complexity) — the `MediaEngine` trait makes this swap clean. See [../../docs/MEDIA.md](../../docs/MEDIA.md).

## Native UX commitments (iOS HIG)
- Native navigation, share sheet, document picker, dynamic type, system appearance.
- CallKit integration for calls; push notifications (APNs).

## Packaging
App Store.
