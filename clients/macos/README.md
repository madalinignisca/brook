# macOS client — Swift + SwiftUI / AppKit

Native macOS app over the shared Rust [`core`](../../core). Likely the **second** client (validates the binding strategy; shares most code with iOS).

## Stack
- **SwiftUI** (with AppKit where needed for native windowing/menus).
- `core` via **UniFFI**-generated Swift bindings.
- Media: platform WebRTC + **VideoToolbox** (HW H.264/HEVC), or GStreamer via the `MediaEngine` trait — decided during the calls phase. See [../../docs/MEDIA.md](../../docs/MEDIA.md).

## Native UX commitments (macOS HIG)
- Real menu bar, keyboard shortcuts, native traffic-light windowing, sidebar style, system file picker (`NSOpenPanel`).
- System appearance (light/dark/accent) followed natively.
- Native notifications, dock badge.

## Packaging
Signed + **notarized** `.app` / `.dmg`.
