# macOS client — Swift + SwiftUI / AppKit

Native macOS app over the shared Rust [`core`](../../core), through the UniFFI bindings in
[`bindings/apple`](../../bindings/apple) (`BrookCore` Swift package).

## Build & test

Apple Silicon, macOS 26+, Xcode, `rustup` (+ `aarch64-apple-darwin`), XcodeGen.

```bash
clients/macos/build.sh          # fresh BrookCore xcframework → xcodegen → xcodebuild
clients/macos/build.sh test     # + unit tests (60 s cap per test, so a deadlock fails)
```

`Brook.xcodeproj`, `Info.plist` and `Brook.entitlements` are **generated** from
[`project.yml`](project.yml) — edit that file, never the generated ones.

Rendered screenshots of the views (light + dark, off-screen, no window):
`TEST_RUNNER_BROOK_SCREENSHOTS=1 xcodebuild … test -only-testing:BrookTests/ScreenshotRenderer`
(written to the app container's `tmp/brook-screens`, the test host is sandboxed).

## Shape
- `SessionStore` — the only owner of `BrookCore`; phase follows `login`'s result.
- `LoginForm` / `LoginView` / `SignedInView` — SwiftUI, native controls only.
- `ServerAddress` / `Settings` — address validation, remembered server, hidden plain-http opt-in.

Specs: [design](../../docs/superpowers/specs/2026-09-24-macos-phase0-app-design.md) ·
[plan](../../docs/superpowers/specs/2026-09-24-macos-phase0-app-plan.md).

## Platform commitments (macOS HIG)
- Real menu bar, keyboard shortcuts, native windowing, system file picker (`NSOpenPanel`).
- System appearance (light/dark/accent) followed natively. Native notifications, dock badge.
- App Sandbox (outgoing network only), hardened runtime; arm64 only.
- Media (calls): libwebrtc + VideoToolbox (H.264) + Voice Processing I/O (echo cancellation).
  See [../../docs/MEDIA.md](../../docs/MEDIA.md).

## Packaging
Signed + **notarized** `.app` / `.dmg` (Developer ID) — planned.
