# macOS Phase 0 app — implementation plan

> Status: **draft** — awaiting review · 2026-09-24
> Implements: [2026-09-24-macos-phase0-app-design.md](2026-09-24-macos-phase0-app-design.md) (approved)
> Branch: `feat/macos-phase0-app`, stacked on `feat/apple-ffi-bridge` (PR #7). Rebased onto `main`
> once #7 merges. Acceptance (spec §2.3) also needs PR #8 (core: no redirects) merged into the base.

Each task ends in a check; tests are written first and seen failing under their named mutation.

### M1 — Project skeleton
- `clients/macos/project.yml`, `generate.sh`, `Brook/BrookApp.swift` (empty window), `Brook/Info.plist`
  keys via XcodeGen (`NSLocalNetworkUsageDescription`), `Brook/Brook.entitlements` (sandbox +
  `network.client`), `BrookTests/` placeholder; `.gitignore` for `Brook.xcodeproj`, `build/`.
- **Check:** from a clean tree: `bindings/apple/build-xcframework.sh && clients/macos/generate.sh &&
  xcodebuild -scheme Brook build` succeeds; `codesign -d --entitlements - Brook.app` shows exactly
  sandbox + network.client; `lipo -archs` = `arm64`.

### M2 — `ServerAddress` + `Settings` (pure logic)
- `ServerAddress.parse(_:) -> Result<String, AddressError>`: trim, require scheme + host, reject
  userinfo/query/fragment. `Settings`: resolves server prefill (env → defaults → `https://localhost`),
  insecure flag (env `BROOK_ALLOW_INSECURE_HTTP=1` or default `AllowInsecureHTTP`), last-good save —
  all over an injected `UserDefaults` + environment dictionary.
- **Check:** spec §5 rows for address rejection and flag resolution, each mutation-verified.

### M3 — `SessionStore` + `FakeClient`
- Store per spec §3.3 with injected factory; `LoginError` → message mapping in one function.
- `FakeClient: FfiBrookClient, @unchecked Sendable` via `init(noHandle:)`, overriding `login` and
  `subscribe`; call log + login gate in a `Mutex`; the gate suspends **outside** `withLock`.
- **Check:** remaining spec §5 rows, each mutation-verified; `xcodebuild test` green.

### M4 — Views + commands
- `LoginView`, `SignedInView`, insecure warning line, default button, disabled + spinner, error text,
  password cleared after each attempt; single `Window`, standard commands.
- **Check:** build + tests green; app launched **from Finder** (`open build/.../Brook.app`) and
  screenshotted in: empty login, validation error, insecure warning (with the default set), and — once
  the test server is up — wrong password and signed-in. Light and dark appearance.

### M5 — Docs + PR
- `docs/user-guide.md`: macOS section (build/run, server field, the `defaults write/delete` opt-in,
  Local Network prompt). `clients/macos/README.md`: build steps.
- **Check:** PR opened as draft; leaves draft when spec §2.3 is satisfied against the test server.

## Where this fails
| Point | Failure | Response |
|---|---|---|
| M1 | XcodeGen's generated Info.plist/entitlements keys wrong for sandbox | verify with `codesign -d --entitlements -` and `plutil -p` in the check, not by eye |
| M3 | `open class` overrides of `async throws` methods rejected in Swift 6 | Codex type-checked this shape; if the real generated signature differs, fall back to the injected factory returning a protocol-typed wrapper |
| M4 | Local Network prompt never appears for ad-hoc builds / appears per build | expected in dev (spec §6); verify on the Finder-launched build, note in PR |
| M4 | acceptance blocked by no test server / PR #8 not merged | ship M1–M4 as draft; acceptance screenshots added when both exist |

## If it stops halfway
Only `clients/macos/`, the two doc files and `.gitignore` change. Nothing shared is touched; the
branch can be dropped without affecting #6/#7/#8.
