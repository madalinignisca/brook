# core call signaling (C1b) — implementation plan

> Status: **approved** (round 1 applied; round 2 skipped — see log) · 2026-09-24
> Implements: [2026-09-24-core-call-signaling-design.md](2026-09-24-core-call-signaling-design.md) (approved)
> Branch: `feat/core-call-signaling` (off `main` @ d74041a). Heavy dial: both reviewers at this gate.

Test-first throughout: every test in design §6 is written before its code and seen failing under its
named mutation. Test infrastructure first, because every later task depends on it.

### P0 — Test harness
- **One test origin** (because `BrookClient` derives REST and `/ws` from one base URL): an `axum`
  dev-dependency server on 127.0.0.1 serving `/api/v1/auth/{login,refresh,me}` and a `/ws` upgrade,
  scripted per test — expect frame / send frame(s) / close with code / hold replies and release them
  in a chosen order. Tests go through the public client (`login` → `start_realtime`), never `ws::run`
  directly.
- **Deterministic gates** (test-only hooks, compiled only under `cfg(test)`): (a) a **writer barrier**
  that holds a command after it is accepted and before it is written, with acknowledgements for
  "registered" and "written"; (b) a **reply-consumer gate** that holds the call task before it
  processes the `call.joined` delivered to its mailbox, so the transport provably handles the next
  frame first.
- `tokio` `test-util` dev-feature; timer tests use `start_paused` time, never real sleeps.
- `FakeEngine`: records calls; every async op gated by a per-op barrier; panics on a second `close()`;
  exposes "closed" so a test can observe close **before** releasing a stalled op, then release it and
  assert the completion had no effect (fence).
- A tracing capture layer at `trace`.
- **Check:** a smoke test logs in, connects, gets `ready`, disconnects cleanly.

### P1 — Session epoch/credential revision + refresh fixes (§3.0)
- `SharedSession` → `{epoch, credential_rev, session}` behind the existing `RwLock`, plus a
  `watch<(epoch, credential_rev)>`. `refresh_once` → `Committed | Discarded | Failed` with the
  refresh-token CAS on the 4xx path too. Login/logout/clear bump `epoch`.
- The WS loop closes its socket when `epoch` changes (pending → `Disconnected`).
- A committed session clear (refresh 4xx) publishes `AuthState::LoggedOut` through `state_tx` (the
  refresh path gets access to it), so UIs learn about a mid-session logout.
- **Check:** tests "stale refresh failure keeps the new session", "login as another user closes the
  socket", "discarded refresh does not notify", "refresh 4xx → LoggedOut published"; all existing core
  tests pass; `cargo check -p brook-gnome`.
- **Mergeable only with client recovery:** GNOME and KDE must return to their login screen on a
  mid-session `LoggedOut` (today GNOME only re-enables a hidden button; KDE handles only the initial
  result). That client work is the Linux side's; P1 merges once it confirms both clients recover at runtime.

### P2 — Transport command path (§3.1)
- `Conn` watch `{generation, ready}`; outgoing mpsc of `Command {frame, expect, generation, reply,
  route: Option<Mailbox>}`; pending map; write-then-arm 10 s timeout; reply-type validation;
  `NotSent | Unknown | Replied`; 64 KiB local cap; route install + `call.joined` delivered to the mailbox
  before the next read; abandoned-join skip/leave; `Secret` for tokens; type-only frame logs.
- `ServerEvent::Ready` unchanged; `ServerEvent::ChannelCall` added.
- Explicit generation rule: every command carries the generation it was created for; the writer drops
  any command whose generation is not current (`Disconnected`).
- **Check:** the transport rows of design §6, plus: server releases two held replies in **reverse** order
  → each command gets its own; a command held at the writer barrier while time advances 30 s, then
  released → it still gets a full 10 s reply window (proves timeout-from-write); the connection changes
  while a command is held → it is never written; back-to-back `call.joined` + offer with the
  reply-consumer gate closed → the offer is in the mailbox (and the "route installed by the call task"
  mutation provably loses it).

### P3 — Re-auth + 1008 recovery (§3.2)
- Re-auth on `credential_rev` change while ready; deferred re-auth if the change happened before ready;
  single-flight refresh shared by `refresh_loop` and the 1008 path; 4xx → clear (epoch bump).
- **Check:** re-auth rows of §6, including 1008 `token_expired` → refresh before reconnect; each re-auth
  test also asserts the connection **generation is unchanged** and no `call.resume` is written.

### P4 — Call types, engine trait, call task (§3.3–3.4)
- `call.rs`: wire types with exact serde names; `MediaEngine`; `CallHandle`; the call task with
  per-PC sequence numbers, publish states incl. `ApplyingAnswer`, subscribe version states with one
  retained answer, per-mid ICE buffers (incl. index-only candidates), resume, serialized mute, `finish()`.
- **Check:** the call rows of §6, each mutation-verified, plus explicit mutations for the round-2
  requirements: `ApplyingAnswer` + drop → after resume no second offer until the apply completes; a late
  `stale` for v1 does not clear a retained v2 answer, and a late `call.ok` never lowers `acked`; mute
  `Timeout`/`Unknown` → no rollback (only an error reply rolls back); epoch change → active calls end
  `Server("session_changed")` and the engine closes; engine fence → close observed **before** releasing a
  stalled `create_publish_offer`, then released → capture stays stopped. Loop the call tests 50×.

### P5 — Log secrecy + docs + client caps
- The trace-capture test across auth, re-auth, join, resume (raw/hex/base64 token absence), with the
  capture layer installed **the way clients install theirs** plus the required cap, and a second run
  without the cap that must fail (proves the cap is what protects dependency logs).
- `core/README.md`: the call API, the engine contract, and the required `tungstenite` log cap.
- Client caps: GNOME has it (the Linux side, 263fb96); KDE — the Linux side; macOS/bindings install no
  tracing subscriber today (nothing is emitted) — the bindings README states the cap is mandatory if one
  is added.
- **Check:** tests green; README states the cap; GNOME/KDE caps confirmed by the Linux side.

### P6 — Integration and hand-off
- `cargo fmt --all --check`, clippy `-D warnings`, `cargo test` (default members), `cargo deny`
  (no new failures vs main). PR opened; signatures announced to the Linux side (adapter) and the server side.
- **Live acceptance** (design §7) happens with the first engine: the Linux side's GTK adapter or the macOS
  engine (C2) against the test server, with the browser harness as the other party.

## Where this fails

| Point | Failure | Response |
|---|---|---|
| P0 | server-side flushing alone cannot force the routing race (client scheduling decides) | the reply-consumer gate holds the call task; the delayed-route mutation must provably lose the offer |
| P1 | epoch change while a REST chat call is in flight | the in-flight REST call finishes with the old token (server may 401); acceptable, not part of calls |
| P2 | tungstenite split sink/stream + select! cancellation drops a half-written frame | writes go through a single writer task; select! only on channel receives |
| P4 | test flakiness from real timers | tokio `start_paused` time for timeout tests; barriers, never sleeps, for ordering |
| P4 | engine futures outliving the call | `finish()` + engine fence; FakeEngine asserts no op completes with effect after close |
| P6 | KDE can't be built here (Qt) | `cargo check` of KDE is done by the Linux side; core changes are additive for it except P1's runtime behaviour |

## If it stops halfway
Each task leaves core compiling with all tests green. P1 is mergeable only together with the GNOME/KDE
login-recovery change (see P1). P2–P3 rewrite the WS loop both chat clients already use, so every task
carries the full existing core test suite plus `cargo check -p brook-gnome` and a GNOME runtime chat
check by the Linux side before its merge — not just P1. P4 adds new API; no client changes source until it
adopts it.

## Review log
**Round 1 — Codex + Vibe (Heavy).** All accepted: P1 publishes `LoggedOut` and needs client recovery to
be mergeable; single-origin axum test server through the public client; deterministic writer barrier
and reply-consumer gate (the originally proposed "write both frames" did not force the race);
`tokio` test-util; explicit generation drop rule; re-auth keeps the generation; explicit mutations for
the round-2 design requirements and the engine fence observed before release; client log caps as real
tasks; halfway claim narrowed. Session-change call termination is checked in P4 (calls don't exist in
P1). Round 2 skipped: every change adds checks, none was disputed.
