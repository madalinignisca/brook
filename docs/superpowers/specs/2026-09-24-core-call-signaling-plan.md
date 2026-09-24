# core call signaling (C1b) — implementation plan

> Status: **draft** — awaiting review · 2026-09-24
> Implements: [2026-09-24-core-call-signaling-design.md](2026-09-24-core-call-signaling-design.md) (approved)
> Branch: `feat/core-call-signaling` (off `main` @ d74041a). Heavy dial: both reviewers at this gate.

Test-first throughout: every test in design §6 is written before its code and seen failing under its
named mutation. Test infrastructure first, because every later task depends on it.

### P0 — Test harness
- `core/tests/support/` (or `#[cfg(test)] mod harness`): an in-process WS server on 127.0.0.1
  (`tokio_tungstenite::accept_async`) scripted per test — expect frame / send frame / close with code,
  with **no implicit yields** so back-to-back sends are really back-to-back; a wiremock REST server for
  `/auth/login`, `/auth/refresh`, `/auth/me`; a `FakeEngine` (records calls; each async op gated by a
  per-call barrier the test releases; panics on a second `close()`; after `close()` every op returns an
  error); a tracing capture layer at `trace` for core's targets.
- **Check:** a smoke test logs in, connects, gets `ready`, disconnects cleanly.

### P1 — Session epoch/credential revision + refresh fixes (§3.0)
- `SharedSession` → `{epoch, credential_rev, session}` behind the existing `RwLock`, plus a
  `watch<(epoch, credential_rev)>`. `refresh_once` → `Committed | Discarded | Failed` with the
  refresh-token CAS on the 4xx path too. Login/logout/clear bump `epoch`.
- The WS loop closes its socket when `epoch` changes (pending → `Disconnected`).
- **Check:** tests "stale refresh failure keeps the new session", "login as another user closes the
  socket", "discarded refresh does not notify"; **all existing core tests still pass**; GNOME and KDE
  build unchanged (`cargo check -p brook-gnome`; KDE needs Qt → checked by brook-linux).
- Behaviour change for GNOME (socket reconnects on login) → announced to brook-linux before merge.

### P2 — Transport command path (§3.1)
- `Conn` watch `{generation, ready}`; outgoing mpsc of `Command {frame, expect, generation, reply,
  route: Option<Mailbox>}`; pending map; write-then-arm 10 s timeout; reply-type validation;
  `NotSent | Unknown | Replied`; 64 KiB local cap; route install + `call.joined` delivered to the mailbox
  before the next read; abandoned-join skip/leave; `Secret` for tokens; type-only frame logs.
- `ServerEvent::Ready` unchanged; `ServerEvent::ChannelCall` added.
- **Check:** the transport rows of design §6 (reverse-order replies, wrong type, not-ready →
  `Disconnected` with nothing written later, in-flight drop → `Unknown`, timeout from write, back-to-back
  joined + offer, abandoned join, unknown call_id).

### P3 — Re-auth + 1008 recovery (§3.2)
- Re-auth on `credential_rev` change while ready; deferred re-auth if the change happened before ready;
  single-flight refresh shared by `refresh_loop` and the 1008 path; 4xx → clear (epoch bump).
- **Check:** re-auth rows of §6, including 1008 `token_expired` → refresh before reconnect.

### P4 — Call types, engine trait, call task (§3.3–3.4)
- `call.rs`: wire types with exact serde names; `MediaEngine`; `CallHandle`; the call task with
  per-PC sequence numbers, publish states incl. `ApplyingAnswer`, subscribe version states with one
  retained answer, per-mid ICE buffers (incl. index-only candidates), resume, serialized mute, `finish()`.
- **Check:** the call rows of §6, each mutation-verified; loop the call tests 50× for flakiness.

### P5 — Log secrecy + docs
- The trace-capture test across auth, re-auth, join, resume (raw/hex/base64 token absence).
- `core/README.md`: the call API, the engine contract, and the required `tungstenite` log cap.
- **Check:** test green; README states the cap.

### P6 — Integration and hand-off
- `cargo fmt --all --check`, clippy `-D warnings`, `cargo test` (default members), `cargo deny`
  (no new failures vs main). PR opened; signatures announced to brook-linux (adapter) and the server side.
- **Live acceptance** (design §7) happens with the first engine: brook-linux's GTK adapter or the macOS
  engine (C2) against the test server, with the browser harness as the other party.

## Where this fails

| Point | Failure | Response |
|---|---|---|
| P0 | a scripted WS server that yields between frames hides the back-to-back race | harness writes both frames to the sink before flushing; the race test asserts on that |
| P1 | epoch change while a REST chat call is in flight | the in-flight REST call finishes with the old token (server may 401); acceptable, not part of calls |
| P2 | tungstenite split sink/stream + select! cancellation drops a half-written frame | writes go through a single writer task; select! only on channel receives |
| P4 | test flakiness from real timers | tokio `start_paused` time for timeout tests; barriers, never sleeps, for ordering |
| P4 | engine futures outliving the call | `finish()` + engine fence; FakeEngine asserts no op completes with effect after close |
| P6 | KDE can't be built here (Qt) | `cargo check` of KDE is done by brook-linux; core changes are additive for it except P1's runtime behaviour |

## If it stops halfway
Each task leaves core compiling with all tests green: P1 is a self-contained bugfix (mergeable on its
own); P2–P3 add transport capabilities nothing calls yet; P4 adds new API. Nothing is exported to
clients until P4, and no client changes source until it adopts the new API.
