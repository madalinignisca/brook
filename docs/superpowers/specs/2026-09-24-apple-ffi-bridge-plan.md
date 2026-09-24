# Apple FFI bridge — implementation plan

> Status: **approved** (review rounds 1–2 closed) · 2026-09-24
> Implements: [2026-09-24-apple-ffi-bridge-design.md](2026-09-24-apple-ffi-bridge-design.md) (approved)
> Branch: `feat/apple-ffi-bridge`

Tasks run in order; each ends in a check that must be seen passing (and, for tests, seen
**failing** first under the named mutation) before the next starts. Test-first within each task.
Test numbers refer to the rows of the spec's §3.2 table (1 runtime hop, 2 tokens, 3 rejected,
4 stalled listener, 5 no duplicates, 6 initial snapshot, 7 cancellation, 8 insecure-http flag).

**Linux gate (`LG`)** — *used once at T1; retired afterwards by owner decision (this machine builds
Apple arm64 targets only); GitHub Actions `rust.yml` is the Linux check from here on.* Was: an `ubuntu:24.04` container (same distro as
`ubuntu-latest`; GTK 4.14 / libadwaita 1.5 satisfy GNOME's `v4_10` / `v1_4` features — Debian
Bookworm's 4.8 / 1.2 do not) with `libgtk-4-dev libadwaita-1-dev`, rustup stable + `rustfmt`
`clippy`, and `cargo-deny`, running exactly `rust.yml`'s steps:
`cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`,
`cargo test --workspace --locked`, `cargo deny check advisories bans sources`.
Cargo's target dir is a container volume so it never mixes with the Mac's `target/`.

**Rebuild rule:** any change under `bindings/apple/src` or to `Cargo.lock` after T4 invalidates
Swift evidence. The Swift checks always go through `itest.sh`, which **runs
`build-xcframework.sh` first**, so no Swift result is ever produced against a stale framework.

## Tasks

### T1 — Crate skeleton, workspace admission, lockfile, CI wiring
- `bindings/apple/Cargo.toml`: `brook-ffi`, `crate-type = ["staticlib", "cdylib", "lib"]`,
  deps `brook-core` (path), `uniffi = "=0.32.2"` (exact pin), `tokio` (`rt-multi-thread`, `time`,
  `net`, `sync`); feature `cli = ["uniffi/cli"]`; `[[bin]] uniffi-bindgen` with
  `required-features = ["cli"]`. Dev-deps: `wiremock`, `futures`, `tokio` (macros).
- `src/lib.rs`: `uniffi::setup_scaffolding!();` only.
- Root `Cargo.toml` `members` += `"bindings/apple"`; `Cargo.lock` regenerated and committed in
  the **same commit**.
- `rust.yml`: `"bindings/**"` added to both `paths:` lists.
- **Check:** Mac `cargo build -p brook-ffi` and `--features cli --bin uniffi-bindgen-swift` succeed;
  **then `LG`**: fmt, Linux clippy and tests pass, and `cargo deny` reports **no error that
  `origin/main` does not already report** (the all-features tree incl. `cli` is exercised here, not
  first at T7).
- **Baseline (found at T1):** `cargo deny` already fails on `origin/main` — RUSTSEC-2026-0285
  (rustls), RUSTSEC-2026-0258 (h2), and a wildcard in `brook-gnome`. None of these are Apple-owned:
  reported to the server session, GNOME part parked for the GNOME session. This branch does not
  touch them; `bindings/apple` adds only two duplicate-version *warnings* (`getrandom`, `syn`).

### T2 — Runtime, config, errors, login (tests 1, 2, 3, 8)
- `runtime()` → `OnceLock<Runtime>` (2 workers, `enable_all`).
- `FfiBrookClient::new(base_url, allow_insecure_http) -> Result<Arc<Self>, LoginError>`.
- `async fn login(handle, password) -> Result<LoginResult, LoginError>` via
  `runtime().spawn(...).await`; `JoinError` → `LoginError::UnexpectedResponse` (a panic in core
  must not cross the FFI as a crash).
- `FfiSession`, `FfiUser`, `FfiAuthState`, `LoginResult`, `LoginError` + `From` impls.
- Mutations: (1) remove the spawn hop → panic; (2) swap token fields → sentinel mismatch;
  (3) map API errors to `UnexpectedResponse` → fails; (8) hard-code `allow_insecure_http = true` →
  the `false` case stops returning `InsecureServerUrl` → fails.

### T3 — Listener + Subscription (tests 4, 5, 6, 7)
- `AuthStateListener` foreign trait; `subscribe()` → `Arc<Subscription>`; delivery task per spec
  §3.1 (`borrow_and_update`, clone, drop borrow, check `AtomicBool`, call).
- A `pub(crate)` constructor taking a `watch::Receiver<AuthState>` so tests drive states directly
  (not exported over FFI).
- Blocking-listener tests use `std::sync::mpsc`/`Barrier` with timeouts — **no `sleep`-based
  ordering**, so they are deterministic on CI.
- **Check:** each test fails under its spec mutation, then passes; the file run 50× in a loop to
  smoke out flakiness.

### T4 — xcframework + Swift package
- `build-xcframework.sh` per spec §3.3; outputs gitignored (`.gitignore` entries added).
- `swift/BrookCore/Package.swift`; hand-written `Sources/BrookCore/`:
  - `Redaction.swift` — `CustomStringConvertible` + `CustomDebugStringConvertible` +
    `CustomReflectable` for `FfiSession` and `LoginResult`.
  - `AuthStateObserver.swift` — the Swift side of cancellation (spec §3.1): wraps a `Subscription`,
    holds a lock-protected `cancelled` flag set **before** calling `Subscription.cancel()`, and
    drops any `onState` that arrives after it is set. This is the object Step 3's `@Observable`
    store builds on.
- **Check:** script run twice (idempotent); the xcframework `Info.plist` lists exactly 1 library
  (macos-arm64 only for now); `swift build` succeeds.

### T5 — Swift tests (written here, executed through `itest.sh` in T6)
- `RedactionTests` (no server): sentinel tokens never appear in `print`/`debugPrint`/`dump`/
  `String(reflecting:)` of both types. Mutation: delete `CustomReflectable` → `dump` test fails.
- `AuthStateObserverTests` (no server): feed the observer's listener directly — deliver, cancel,
  then deliver a late state → observer's recorded states exclude it. Mutation: drop the
  `cancelled` check → fails.
- `LoginIntegrationTests`: the three spec §2.3 scenarios; `/auth/me` via `URLSession`. With
  `BROOK_REQUIRE_ITEST=1` and env missing → `XCTFail`.
  Swap mutation: swap the Rust `From<Session>` fields, rebuild (via `itest.sh`) → `/auth/me` with
  the "access" token (actually the refresh token) returns 401 → fails. Distinctness alone is not
  relied on.
- **Check:** unit tests pass via `swift test --filter` after `build-xcframework.sh`; integration
  tests are only considered proven once T6 runs them.

### T6 — `itest.sh` against an isolated stack
- **Env:** a generated scratch `--env-file` (mktemp, mode 600, deleted on exit) with random
  `POSTGRES_PASSWORD`, a random 64-char `BROOK_JWT_SIGNING_KEY`, and `MINIO_ROOT_PASSWORD` (the
  `minio` service is profiled off but still interpolated). Never reads `deploy/.env`; never calls
  `deploy/Makefile`.
- **Files:** `-f <abs>/deploy/docker-compose.yml -f <scratch override>` — deploy file **first**, so
  the api build context (`../services/api`) and Caddy mount resolve relative to `deploy/`.
  Override: `caddy.ports: !override ["127.0.0.1:18080:80"]`.
- **Fresh state:** project name `brook-itest-<random>` per run, so volumes are unique; teardown
  (`trap`, identical `-p`/`-f`/`--env-file` args) runs `down -v --remove-orphans`. **No sweep of
  other runs' projects** — a prefix cannot tell an interrupted run from a live one. If teardown
  itself fails, the script prints the exact `down -v` command for its own project and exits non-zero.
- **Startup:** `up -d --build --wait` (fresh API image from current `services/api`), assert Compose
  ≥ 2.24.4 (first release with `!override`) **before** `up`, assert the only published binding is `127.0.0.1:18080`, `/health` (timeout 60 s),
  register (non-201 aborts).
- **Run:** `build-xcframework.sh`, then `swift test` (plain env) on macOS; fail on any skip or fewer
  integration tests than expected. (Simulator run with `TEST_RUNNER_` vars returns with iOS.)
- **Check:** passes end-to-end; a run with the api container killed mid-run fails (not skips);
  after both, `docker compose ls -a --filter name=<this run's project>` and
  `docker volume ls --filter label=com.docker.compose.project=<this run's project>` are empty —
  scoped to this run, so an unrelated developer stack never affects the result.

### T7 — Final Linux gate
- Re-run `LG` on the final tree. If it forces a dependency change (e.g. an advisory bump), re-run
  **T6 in full** afterwards — the Apple evidence is only valid for the lockfile it ran against.
- **Check:** `LG` and (if re-run) `itest.sh` output pasted into the PR.

### T8 — Conclusion
- `docs/JOURNAL.md` entry (repo convention) + PR body: what's built, what's skipped, what's still
  unproven; announce to the server session that `bindings/apple` is in the workspace.

## Where this fails

| Point | Likely failure | Response |
|---|---|---|
| T1 | UniFFI 0.32 vs rustc 1.98 / tokio version skew | pin the newest 0.32.x that builds; record the pin reason beside it |
| T1 | `cargo deny` advisory in UniFFI's `cli` tree | fix by version bump; never add to `ignore` without an owner decision |
| T3 | a blocked Swift listener occupies a Tokio worker and stalls login | 2 workers; if tests show a stall, deliver via `spawn_blocking` — design unchanged |
| T4 | static lib needs system frameworks at link time | `reqwest` has no default features (no `system-configuration`); if the link fails, add `linkerSettings` |
| T6 | api image build slow / network-dependent | `--build` is required for correctness; accept the time |
| T6 | no simulator runtime available | script fails loudly naming the missing runtime |

## If it stops halfway

T1 is the first commit and adds no new Linux-gate failure over the `origin/main` baseline, so any
later stopping point leaves CI no worse for other contributors. Rollback: revert the T1 commit (members
line, `Cargo.lock`, `rust.yml` filter) together — removing the members line alone would leave a
stale lockfile that `--locked` rejects. T2–T6 add files only under `bindings/apple/` plus
`.gitignore` entries. No task edits `core/`, `services/`, `deploy/`, or GNOME.

## Review log

**Round 1 — Codex.** Seven findings, all accepted: Linux gate moved to Ubuntu 24.04 (Bookworm's
GTK/libadwaita are too old for GNOME) and run at T1, with the lockfile committed in the same
commit; complete rollback recipe; `itest.sh` env-file, file ordering, `--build`, per-run project
names, and project-scoped cleanup checks; rebuild rule so Swift never tests a stale framework, and
T6 re-run after any late dependency change; test numbering corrected (config test = 8);
Swift-side cancellation guard (`AuthStateObserver`) added with its own test.

**Round 2 — Codex.** Six of seven resolved. Fixed: cleanup assertions scoped to this run's project
(#4); stale-project sweep removed (unsafe — cannot tell interrupted from live runs); Compose
minimum raised to 2.24.4 and checked before `up`. Plan gate closed.

**Scope change (owner):** macOS first — T4/T6 build and test the macOS arm64 slice only; iOS slices and
the simulator run move to the iOS client. T7 becomes: push, and read `rust.yml` on GitHub (the
`deny` job's pre-existing GNOME-wildcard failure is the baseline). Next step after T8 is the macOS
app shell, with a human test against the server-side test environment.
