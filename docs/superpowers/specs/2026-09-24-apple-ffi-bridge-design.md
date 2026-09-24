# Apple FFI bridge (Step 1 of the macOS/iOS clients)

> Status: **approved** (review rounds 1–2 closed) · 2026-09-24
> Review dial: **Standard** (one external model per gate). No secrets are persisted in this step;
> Keychain storage is Step 2 and is reviewed at **Heavy**.
> Reviewed by: Codex — see §6.
> Touches: [CLIENT_PHILOSOPHY.md](../../CLIENT_PHILOSOPHY.md) (FFI state-observation pattern),
> [PROTOCOL.md](../../PROTOCOL.md) §1 (`/auth/login`), `.github/workflows/rust.yml`, root `Cargo.toml`

## 1. Problem

The macOS and iOS clients are thin SwiftUI layers over `brook-core`. `brook-core` exposes a
Rust-idiomatic async API (`BrookClient::login`, `tokio::sync::watch::Receiver<AuthState>`).
Two things stop Swift from using it as-is:

- UniFFI has no mapping for a `watch` channel, so state observation needs a callback contract.
- UniFFI async exports are polled by the **foreign** executor (Swift's), which is fine in
  general — but `brook-core` uses `reqwest`, which needs a **Tokio reactor and timers**. Those
  futures must run on a Tokio runtime the wrapper owns.

Before any Swift UI exists, one question must be answered with running code: **can Swift
drive `brook-core` end-to-end — call `login`, observe state, receive intact tokens — on both
macOS and the iOS simulator?** Everything later (Keychain, UI, chat) rests on this.

## 2. Done means (observable)

1. The full existing Rust CI gate passes **on Linux** (CI is `ubuntu-latest`) with the new crate
   in the workspace: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`,
   `cargo test --workspace --locked`, `cargo deny check advisories bans sources`. `Cargo.lock` is updated
   and committed.
2. `bindings/apple/build-xcframework.sh` produces `BrookCoreFFI.xcframework` per the artifact
   contract in §3.3, consumed by the local Swift package `BrookCore`.
3. `bindings/apple/itest.sh` exits 0, which requires **all** of the following, run against the shared
   test server (§3.4) — on macOS (`swift test`). (iOS slices and the simulator run are deferred: macOS ships first.)
   - `login` with valid credentials returns `LoginResult.loggedIn(session)` whose `user.handle`
     matches, whose `accessToken` and `refreshToken` are non-empty and different, and whose
     `accessToken` **works against the server** (`GET /api/v1/auth/me` from Swift → 200, same handle).
   - The listener's **final** observed state is `loggedIn(user)`, and the observed sequence never
     regresses (no `authenticating` after `loggedIn` within one login).
   - A wrong password yields `LoginError.Api(code: "auth.invalid_credentials", …)` and a final
     `failed` state.
   - Zero integration tests skipped: the script sets `BROOK_REQUIRE_ITEST=1`, under which a missing
     server/credential environment is an **`XCTFail`**, not a skip.
4. Each new test is watched failing on purpose before it is trusted, with the mutation named in §5.

## 3. Design

### 3.1 A separate wrapper crate: `bindings/apple` (`brook-ffi`)

`brook-core` is **not modified** in this step. The GNOME client consumes it directly and its
API stays idiomatic Rust. `brook-ffi` depends on `brook-core` and owns all UniFFI concerns.
UDL-free, proc-macro UniFFI (`uniffi::setup_scaffolding!()`), pinned to an exact 0.32.x release (the review referenced 0.29; 0.32.2 was current at implementation and keeps every API used here: `with_foreign`, async exports, the `cli` feature, library-mode bindgen, plus a Swift-specific bindgen).

- **Runtime:** one process-wide multi-thread Tokio runtime (`OnceLock<Runtime>`), 2 worker
  threads, built with `enable_io()` + `enable_time()` (i.e. `enable_all()`). Every exported async
  fn does `runtime().spawn(fut).await` on the `JoinHandle`, so it is correct no matter which
  executor polls it.
- **Observation — latest-state contract.** The listener is a foreign trait:

  ```rust
  #[uniffi::export(with_foreign)]
  pub trait AuthStateListener: Send + Sync { fn on_state(&self, state: FfiAuthState); }
  ```

  `FfiBrookClient::subscribe(Arc<dyn AuthStateListener>) -> Arc<Subscription>` spawns one task
  that delivers the current value, then loops on `changed()`. Reads use `borrow_and_update()`,
  **clone and drop the borrow before** calling foreign code (no duplicate notifications, no
  lock held across the FFI call).

  Semantics, stated in the trait's doc comment: *latest state wins.* `watch` is single-slot,
  so a slow listener may skip intermediate states (e.g. never see `Authenticating`), but
  **observed order never regresses** (one task, synchronous callbacks) and the **final state is
  always delivered**. UIs render the current state; they must not count transitions.
- **Cancellation is a request.** `Subscription::cancel()` (also on `Drop`) sets an `AtomicBool`
  checked before every callback, then `abort()`s the task. `cancel()` does not wait.
  Guarantee: after `cancel()` returns, **at most one** further callback may be delivered (one
  whose task passed the check before the flag was set); never two. The check-then-call is not
  synchronized with `cancel()` on purpose — a blocking cancel would need a lock shared with the
  callback and could deadlock against a Swift callback that hops synchronously to the main
  thread while the main thread cancels. The Swift wrapper drops any state delivered after it
  cancelled.
- **Login result as an enum from day one.** Phase 0b adds a second `/auth/login` response
  (`totp_required` + pending token). To keep that additive for the FFI surface:

  ```rust
  #[derive(uniffi::Enum)]
  pub enum LoginResult { LoggedIn { session: FfiSession } /* TotpRequired { pending_token } in 0b */ }
  ```

  Today `brook-core` returns `Session`, mapped to `LoggedIn`. Swift switches exhaustively, so
  0b's new case is a compile-time prompt in every client rather than a silent runtime path.
- **Errors:** `brook_core::Error` → `#[derive(uniffi::Error)] enum LoginError { Network{message},
  InvalidServerUrl{message}, InsecureServerUrl, Api{code, message}, UnexpectedResponse }`.
  `code` is preserved verbatim; UIs key off it.
- **Config:** `FfiBrookClient::new(base_url, allow_insecure_http)` → `CoreConfig::with_options`,
  behaviour unchanged from core: `https` always allowed; `http` allowed to loopback hosts; with
  `allow_insecure_http = true`, `http` to **any** host is allowed — which sends the password and
  receives tokens in cleartext. That flag is dev-only; the Step 3 UI must gate it behind an
  explicit, labelled opt-in. Tested with both flag values at the FFI boundary.
- **Tokens across the boundary:** `FfiSession` carries `access_token`/`refresh_token` because
  Step 2 must store them in the Keychain. On the Swift side, `FfiSession` **and** `LoginResult`
  get (in a hand-written file, not the generated one) a redacting `CustomStringConvertible`,
  `CustomDebugStringConvertible` **and `CustomReflectable`** — the last is what `dump()` and the
  debugger's child view use; description conformances alone do not stop `dump()` printing tokens.
  Sentinel test: `print`, `debugPrint`, `dump`, `String(reflecting:)` of both types never contain
  the token strings.

### 3.2 Linux CI stays green

`brook-ffi` has **no Apple-only dependencies or `cfg`s**; UniFFI compiles on Linux. The
`uniffi-bindgen` binary needs UniFFI's `cli` feature, gated with `required-features = ["cli"]`
so the default Linux `--all-targets` build does not pull it; the build script passes
`--features cli`. The workflow's `paths:` filters gain `bindings/**` (push and pull_request).

Rust tests (wiremock, like core's) that run in CI:

| Test | Proves | Mutation it must catch |
|---|---|---|
| login called via `futures::executor::block_on` on a plain `std::thread` (no Tokio entered) | the runtime hop exists | remove `runtime().spawn` → panics "no reactor running" |
| login success → `LoggedIn`, tokens equal wiremock's sentinels (distinct values) | mapping keeps tokens intact, unswapped | swap the two fields → fails |
| rejected → `Api{code: "auth.invalid_credentials"}` | error mapping | map to `UnexpectedResponse` → fails |
| stalled listener (blocks on first callback) → eventually gets `LoggedIn`, order never regresses | latest-state contract | drop the final state → fails |
| listener records every callback; states are driven by the test (via a wrapper constructor that takes a `watch::Receiver`) with a controlled interleaving: value sent while the task is between initial read and `changed()` → **each distinct value is delivered at most once in a row** | no duplicate delivery | `borrow()` instead of `borrow_and_update()` → duplicate → fails |
| subscribe on a **fresh** client before any state change → exactly one callback, `LoggedOut`, within a timeout | explicit initial snapshot (a fresh receiver's `changed()` is not ready, so only the explicit delivery can produce it) | skip initial delivery → no callback → fails |
| cancel while a callback is blocked, then drive two more state changes and release → at most one callback after `cancel()` returned | at-most-one-late guarantee | remove the `AtomicBool` check and the `abort()` → two late callbacks → fails |
| `new("http://192.168.1.50", false)` → `InsecureServerUrl`; `true` → Ok | flag delegation | hard-code `true` → fails |

### 3.3 Apple packaging — artifact contract

```
bindings/apple/
  Cargo.toml  (crate-type = ["staticlib", "cdylib", "lib"])
  src/lib.rs, src/bin/uniffi-bindgen.rs
  build-xcframework.sh
  itest.sh
  swift/BrookCore/                  ← local Swift package (Package.swift checked in)
    Sources/BrookCoreFFI.xcframework  ← build output, gitignored
    Sources/BrookCore/Generated/      ← generated brook_ffi.swift, gitignored
    Sources/BrookCore/Redaction.swift ← hand-written, checked in
    Tests/BrookCoreTests/             ← checked in
```

- `cargo build --release --target aarch64-apple-darwin` → **static** `libbrook_ffi.a`. **arm64 only, Apple targets only** —
  no x86_64 slice is ever built. iOS (`aarch64-apple-ios`, `aarch64-apple-ios-sim`) slices are added
  when the iOS client starts; the script is written so adding them is one list entry.
- Library-mode bindgen from the host `cdylib` → `brook_ffi.swift`, `brook_ffiFFI.h`,
  `brook_ffiFFI.modulemap` (renamed `module.modulemap`).
- `xcodebuild -create-xcframework` with one library entry per slice (macOS arm64 now; iOS arm64 and
  iOS-simulator arm64 later), each `-library <.a> -headers <dir with header + module.modulemap>`.
- `Package.swift`: `.binaryTarget(name: "BrookCoreFFI", path: …xcframework)`; the
  `BrookCore` source target depends on it; platforms `.macOS(.v26)` (tools 6.2; matches `MACOSX_DEPLOYMENT_TARGET=26.0` in the build script), iOS added with the iOS client.
- `set -euo pipefail`; missing target or tool fails loudly; idempotent (cleans its outputs).

Nothing in Step 1 creates an Xcode project.

### 3.4 Integration test against the shared test server

**Superseded (owner decision):** no local Docker stack on the Mac. All clients — macOS from this
machine, Linux later — test against **one real server on the Linux VM**, deployed and operated by
the server session. `itest.sh`:

1. Reads `BROOK_TEST_SERVER`, `BROOK_TEST_HANDLE`, `BROOK_TEST_PASSWORD` and optional
   `BROOK_TEST_ALLOW_INSECURE_HTTP=1` from `bindings/apple/.itest.env` (gitignored, mode 600,
   filled in by the owner — the password never travels through an agent channel or the repo).
   A missing file or variable is a hard failure.
2. Checks `GET /health` on the server; unreachable → fail with the URL it tried.
3. Runs `build-xcframework.sh`, then `swift test` with those values plus `BROOK_REQUIRE_ITEST=1`.
4. Fails if any integration test is skipped or fewer than expected ran.

It does not register accounts or mutate server state beyond logging in: the test account is
created once by the admin. If the server is plain `http`, the tests pass
`allow_insecure_http = true` only when `BROOK_TEST_ALLOW_INSECURE_HTTP=1` is set, mirroring the
GNOME client's `BROOK_ALLOW_INSECURE_HTTP=1`.

## 4. Not doing (this step)

- No change to `brook-core` (refresh, logout, restore, `LoginResult` in core itself) — those
  are Step 2 and are announced to the server session first because GNOME shares core.
- No Keychain, no session persistence, no app targets, no UI, no Xcode projects.
- No Intel (`x86_64`) slices, ever built on this machine; Apple Silicon only.
- No iOS in Step 1: macOS client ships first; iOS slices + simulator tests arrive with the iOS client.
- No Apple CI job yet (macOS runners); added with the app targets in Step 3. Until then, §2.3 is
  run locally and its output pasted into the PR.
- No TOTP/OIDC/LDAP, WebSocket, push, CallKit.
- No change to `deploy/` (the LAN-binding fix is the server session's, pending approval).

## 5. Failure modes & halfway state

| Risk | Consequence | Mitigation |
|---|---|---|
| Futures polled without a Tokio reactor | panic "no reactor running" | runtime hop; tested from a thread with no runtime entered |
| Listener called after Swift deinit | crash on a dangling foreign object | UniFFI holds a strong `Arc` to the foreign listener; `Subscription` drop cancels |
| `watch` coalesces transitions | UI misses `Authenticating` | contract is latest-state; tests assert final state + no regression, never an exact sequence |
| Tokens leak into logs via Swift reflection | secrets in console/crash logs | `CustomReflectable` redaction + sentinel test incl. `dump` |
| Integration tests silently skipped | false green | `BROOK_REQUIRE_ITEST=1` turns skip into failure; script counts executed tests |
| New crate breaks Linux CI | blocks every Rust PR, incl. GNOME | full CI gate (§2.1) run in a Linux container before pushing |
| Stopping halfway | — | only new directories + one workspace member + CI path filter; reverting the member line restores the previous state |

## 6. Review log

**Round 1 — Codex.** All eight findings accepted, none rebutted:
token redaction must cover `dump` (`CustomReflectable`); simulator env must use `TEST_RUNNER_` and
skips must fail the run; listener contract made explicitly latest-state with matching tests and
`borrow_and_update`; cancellation specified as a request (no new callback after return); test
fixture moved to an isolated fresh stack (registration after the first user needs an admin);
runtime-hop test must poll from a thread with no Tokio runtime; tokens verified from Swift against
`/auth/me`; `allow_insecure_http` described accurately (any host). Also adopted: artifact contract
for the xcframework, `required-features` for the bindgen binary, full CI gate in "done".

**Round 2 — Codex.** Seven of eight resolved. Still open: cancellation — "no callback starts
after `cancel()` returns" cannot hold with an unsynchronized flag + `abort()`. Decided by the
owner: **at most one late callback**, non-blocking `cancel()`, Swift wrapper drops late states
(§3.1). New defects fixed: compose override must *replace* ports (`!override`) and the binding is
asserted; the duplicate-delivery and initial-snapshot tests rewritten so their mutations actually
fail them. Spec gate closed.

**Scope change (owner, after approval):** macOS first. iOS slices and the simulator integration run
are deferred to the iOS client; arm64 Apple targets only. The Linux CI gate is not run locally
(Apple-only builds on this machine); GitHub Actions is the Linux check.
