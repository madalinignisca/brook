# macOS client — Phase 0 app

> Status: **approved** (review rounds 1–2 closed) · 2026-09-24
> Prerequisite: `core` must not follow HTTP redirects (fixed separately, `fix/core-no-redirects`) —
> otherwise "core enforces https" below would be false. See §7.
> Review dial: **Standard**. No secret is persisted: like the GNOME client, Phase 0 keeps no session
> across launches. Keychain storage arrives with session restore (needs `core` refresh) and is Heavy.
> Builds on: [2026-09-24-apple-ffi-bridge-design.md](2026-09-24-apple-ffi-bridge-design.md) (`BrookCore`)
> Touches: `clients/macos/` only (+ `.gitignore`), [user-guide.md](../../user-guide.md) (macOS section)

## 1. Goal — parity with the GNOME Phase 0 client

The GNOME client today: server from `BROOK_SERVER` (default `https://localhost`), plain-http opt-in
`BROOK_ALLOW_INSECURE_HTTP=1`, a login form (handle, password, **Log in**, Enter submits, inline
error, empty-field check, no double submit), then a "Signed in" placeholder. No persistence, no
logout. The macOS app does the same, the macOS way.

## 2. Done means (observable)

1. `clients/macos/generate.sh && xcodebuild -scheme Brook build` produces `Brook.app` (arm64, macOS 26+)
   from a clean checkout after `bindings/apple/build-xcframework.sh`.
2. `xcodebuild test -scheme Brook` runs the `BrookTests` unit tests green, each seen failing under
   its named mutation (§5).
3. Launched against the shared test server, a human can: enter the server address, handle and
   password; press Return; see the signed-in view with their display name. A wrong password shows
   "Wrong handle or password." inline and the form stays usable. A screenshot of each state is
   attached to the PR.
4. The launched app is a native macOS window: standard menu bar (Brook / Edit / Window / Help),
   ⌘Q, ⌘W, text editing shortcuts in fields, light/dark and accent colour follow the system.

## 3. Design

### 3.1 Project
- `clients/macos/project.yml` (XcodeGen 2.46, MIT) → `Brook.xcodeproj`, **not committed**
  (generated; `.gitignore`). `generate.sh` checks XcodeGen and the xcframework exist, then runs it.
  Rationale: a committed `.pbxproj` is unreviewable; `project.yml` is.
- Targets: `Brook` (app, SwiftUI lifecycle) and `BrookTests` (unit tests, hosted). Depends on the
  local package `../../bindings/apple/swift/BrookCore`.
- Bundle id `dev.brook.Brook` (same as GNOME's app id). `MACOSX_DEPLOYMENT_TARGET = 26.0`,
  `ARCHS = arm64` only. Swift 6 language mode.
- Signing: ad-hoc (`-`) for dev builds; hardened runtime on. **App Sandbox on** with only
  `com.apple.security.network.client` — a chat client needs nothing else yet; Developer ID +
  notarization is the distribution path (as for the owner's other macOS app), added with packaging.

### 3.2 Configuration
- **Server address**: a field on the login form, prefilled from, in order: `BROOK_SERVER` env var
  (dev), the last address that logged in successfully (`UserDefaults`), else `https://localhost`.
  Deviation from GNOME (which has no field): a Finder-launched Mac app has no environment, so
  without a field the app could only reach `localhost`. **Parked for the GTK client** as a parity
  question, not changed there.
- **Plain http opt-in**: `BROOK_ALLOW_INSECURE_HTTP=1` env var, or the hidden default
  `defaults write dev.brook.Brook AllowInsecureHTTP -bool YES` (the Finder-launch equivalent).
  No visible toggle, matching GNOME. When active, the login form shows a persistent warning line:
  "Insecure connections allowed — your password is sent unencrypted." Core enforces the rule on the
  configured URL and (with the prerequisite fix) never follows a redirect elsewhere. The user guide
  documents how to turn the default off again (`defaults delete dev.brook.Brook AllowInsecureHTTP`).
- **Server address validation** (before building the client and before saving): must parse as a URL
  with scheme and host; **userinfo (`user:pass@`), query and fragment are rejected** with "Enter just
  the server address, like https://chat.example.com" — core would accept userinfo, and saving it to
  `UserDefaults` would persist a secret.

### 3.3 State
`@MainActor @Observable final class SessionStore`, the only owner of `BrookCore` in the app:
- `phase: .signedOut(error: String?) | .signingIn | .signedIn(FfiUser)`.
- `signIn(server:handle:password:)`: trims **server and handle only**; the password is passed
  **exactly as typed** (the server hashes it verbatim; `" secret "` is a valid password) and checked
  with `isEmpty`. Empty field or invalid address → inline error, no client, no network call. Ignores
  calls while `.signingIn`. Sets `.signingIn` before any suspension. Builds the client through an
  injected factory `(server, allowInsecureHttp) throws -> FfiBrookClient` (constructor errors such as
  `InsecureServerUrl` → inline error); awaits `login`.
- **Phase comes only from the `login` result.** No `AuthStateObserver` in Phase 0: nothing but
  `login` can change the state yet, and the result carries the typed `LoginError`, so an observer
  would be a second, redundant source of truth whose main-actor hops are not FIFO. It arrives with
  session restore/logout, when state can change outside a call (that is what BrookCore's tested
  observer is for).
- The last-good server address is saved **only after** a successful login.
- Error text, keyed on `LoginError` (codes verbatim from the server):
  `Api(code: "auth.invalid_credentials")` → "Wrong handle or password."; other `Api` → the server's
  message; `Network` → "Couldn't reach the server. Check the address."; `InsecureServerUrl` →
  "The server address must start with https://"; `InvalidServerUrl` → "That server address isn't
  valid."; `UnexpectedResponse` → "The server sent an unexpected response."
- The password string is not stored on the store and is cleared from the field after an attempt.
- Tests inject a factory returning `FakeClient: FfiBrookClient`, built with UniFFI's
  `init(noHandle:)` mocking hook, overriding **`login` and `subscribe`** (inherited methods would use
  handle 0). Its call log and a gate that suspends `login` until released are guarded by a `Mutex`.
  Defaults are an injected `UserDefaults(suiteName:)` per test.

### 3.4 Views (SwiftUI, native controls only)
- `LoginView`: `Form` (grouped) with Server, Handle, Password (`SecureField`) — `.textContentType`
  set so Passwords autofill works; **Log in** is the default button (Return); disabled + spinner
  while signing in; error below in `.red` secondary text.
- `SignedInView`: `ContentUnavailableView("Signed in as <display name>", …)` — "Chat, calls and
  files will live here.", mirroring GNOME's placeholder.
- One `Window("Brook", id: "main")` (single window, no tabs), default ~420×560, standard commands.

## 4. Not doing
- Session persistence, Keychain, logout, refresh — need `core` work; next step.
- Visible insecure toggle, settings window, sidebar/chat UI (Phase 1).
- Notarized packaging and Apple CI job (with the first human-test hand-off build, separately).
- iOS.
- Any change to `core/`, `services/`, the GNOME client or shared docs other than the macOS section
  of the user guide.

## 5. Tests (unit, `BrookTests`) — each with the mutation it must catch

| Test | Mutation |
|---|---|
| empty handle or empty password → error, factory never called | remove the empty check |
| password `" p w "` reaches `login` byte-for-byte; handle/server are trimmed | trim the password too |
| `https://u:secret@host`, `…?x=1`, `…#f` → rejected, factory never called, nothing in defaults | drop the userinfo/query check |
| first `login` suspended at the gate → second `signIn` ignored (exactly one `login` call) | remove the re-entrancy guard |
| after a failed login, a new `signIn` goes through (not stuck in `.signingIn`) | forget to leave `.signingIn` on error |
| factory throws `InsecureServerUrl` (constructor path) → https message, no `login` call | only map errors thrown by `login` |
| login throws `Api(auth.invalid_credentials)` → "Wrong handle or password."; other `Api` → server message | map all `Api` to server message |
| login suspended at the gate → defaults still hold the **previous** address; after success → new one | save before success |
| failed login → address **not** saved | save on every attempt |
| password field cleared after success **and** after failure | clear only on success |
| resolved insecure flag (env **or** default; neither → false) is what the factory receives | resolve correctly but pass `true` |
| success → `.signedIn(user)` with the fake's user | ignore the result |

Live behaviour (§2.3) is verified by running the app against the shared test server, with
screenshots, not by UI tests — XCUITest against a live server is out of scope for Phase 0.

## 6. Failure modes

| Risk | Mitigation |
|---|---|
| SwiftPM binary target + XcodeGen local package resolution differs from `swift build` | `generate.sh` + `xcodebuild build` is criterion §2.1, checked first |
| Sandbox blocks outgoing sockets | `com.apple.security.network.client` covers reqwest's TCP; ATS does not apply to reqwest at all, so no ATS keys are added |
| **Local Network privacy** (LAN test server) | `NSLocalNetworkUsageDescription` in Info.plist. The first connection can fail before the user answers the prompt, so a `Network` error on a LAN address says "If macOS asked to allow local network access, allow it and try again." Acceptance (§2.3) is done with the app **launched from Finder** (Terminal-launched processes can be exempt); ad-hoc re-signing may re-prompt across builds — expected in dev |

## 7. Review log

**Round 1 — Codex.** P1: "core still enforces the rule" was false — core followed redirects, and a
307/308 re-sends the password body to any `Location`, http included; confirmed with a failing test
and by the server session (FastAPI 307s trailing-slash paths and, behind TLS termination, builds an
`http://` Location). Fixed in `core` as a prerequisite (separate branch, Heavy review). P2s, all
accepted: password never trimmed; server address rejects userinfo/query/fragment before use or
save; concrete fake design (`init(noHandle:)` subclass overriding `login` + `subscribe`, `Mutex`-
guarded); ATS fallback removed, Local Network privacy specified with Finder-launch acceptance;
test table rewritten so each mutation actually fails (constructor-error path, factory receives the
resolved flag, save-before-success via a suspended login, recovery after failure, exact password).
Observer dropped from Phase 0 (Codex: redundant; its hops are not FIFO).

**Round 2 — Codex.** All six resolved (it type-checked the `init(noHandle:)` subclass fake with a
`Mutex` gate against the generated module, Swift 6, warnings as errors). Stale observer row in §6
removed. Implementation notes adopted: fake restates `@unchecked Sendable`, `import Synchronization`,
suspends **outside** `withLock`. The redirect prerequisite (PR #8) must land before app acceptance.
