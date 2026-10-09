# macOS client — Swift + SwiftUI / AppKit

Native macOS app over the shared Rust [`core`](../../core), through the UniFFI bindings in
[`bindings/apple`](../../bindings/apple) (`BrookCore` Swift package).

## Build & test

Apple Silicon, macOS 26+, Xcode, `rustup` (+ `aarch64-apple-darwin`), XcodeGen.

```bash
clients/macos/build.sh          # fresh BrookCore xcframework → xcodegen → xcodebuild
clients/macos/build.sh test     # + unit tests (60 s cap per test, so a deadlock fails)
clients/macos/build.sh install  # Release build, then installs /Applications/Brook.app (quit Brook first)
```

For builds made with `build.sh`, the output goes to `build.noindex/` (Spotlight skips it), so
Spotlight finds the installed app, not a Debug and a Release copy. Builds from the Xcode IDE
still land in DerivedData. A Debug build titles its window "Brook (Debug)" and, being ad-hoc signed
without the keychain group, can't stay signed in; use `install` for the app you live in.

`Brook.xcodeproj`, `Info.plist` and `Brook.entitlements` are **generated** from
[`project.yml`](project.yml) — edit that file, never the generated ones.

Rendered screenshots of the views (light + dark, off-screen, no window):
`TEST_RUNNER_BROOK_SCREENSHOTS=1 xcodebuild … test -only-testing:BrookTests/ScreenshotRenderer`
(written to the app container's `tmp/brook-screens`, the test host is sandboxed).

## Shape
Code the Mac and iOS apps share lives in [`clients/apple-shared`](../apple-shared): the session
store (`SessionStore`, the only owner of `BrookCore`), the sign-in form (`LoginForm`), address
validation and the remembered server (`ServerAddress`, `Settings`), the channel model, and the
conversation models (`TimelineModel`, `ComposerModel`, file staging, pending messages, typing and
search, and the small `ScrollToLatest` and `MessageText` helpers). Its tests run in both apps,
including the timeline, composer, pending, typing and search tests. The Mac's own files include
the SwiftUI views (`LoginView`, `SignedInView`, `ChatView`), the conversation files that stay on
the Mac (`PasteImport`, which needs AppKit and is set on the composer in `ChatView.makeComposer`;
`FileRowModel`, which opens files through `NSWorkspace`; `CacheFeed`, the Mac's local-data feed),
and its platform files (`ThisDevice`, `AppActivity`, `SessionPersistence+Mac`), which fill in the
platform facts that shared code asks for. The rule for those is in the [iOS README](../ios/README.md#what-is-shared-and-what-is-ios-only).

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
