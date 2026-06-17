# Brook — Work Journal

A running, human-readable log of notable work: what changed, why, and — for
substantive changes — the **three-model "friends" review** (Vibe / Codex /
Gemini) that vetted it and which of their findings were applied.

Conventions:

- Newest entries on top.
- Substantive `feat` work records the friends review (models + the real
  findings applied vs. declined). Trivial/doc work just notes what changed.
- **Never put secrets here.** Mask anything that looks like a password, key, or
  token as `******` (the gitleaks pre-commit hook also enforces this repo-wide).

---

## 2026-06-17

### Brook-owned error envelope (server + client core) — reviewed by Codex/Gemini/Vibe

Replaced FastAPI's default `{"detail": ...}` error body with Brook's own
`{"error": {"code", "message", "details?"}}` (per `PROTOCOL.md §5`), so the wire
contract every native client parses is owned by us, not implicitly defined by a
dependency's default that could change across major versions. Global exception
handlers in `services/api/app/errors.py`; the Rust core (`core/src/client.rs`)
parses the new shape.

Friends review (Codex `gpt-5.5`, Gemini CLI 0.46, Vibe CLI 2.14) — applied:

- **HIGH** (Codex, Vibe): no catch-all `Exception` handler → unhandled 500s
  escaped the envelope. Added an `Exception` handler returning
  `{"error":{"code":"internal_error"}}` and logging the traceback.
- **LOW** (Codex): `HTTPBearer(auto_error=True)` returned `403` for a *missing*
  token. Switched to `auto_error=False` → a missing token is now
  `401 auth.unauthorized`. Locked with a test.

### Alembic migrations + in-place upgrades — reviewed by Codex/Gemini/Vibe

The API now owns its schema via Alembic instead of `create_all`. The container
entrypoint runs `alembic upgrade head` on start (idempotent) and
`BROOK_AUTO_CREATE_SCHEMA=false` in the deployed stack, so a persistent server
(the staging VM) can be **upgraded in place without wiping data**.

Friends review (Codex `gpt-5.5`, Gemini CLI 0.46, Vibe CLI 2.14) — applied:

- **HIGH** (all three): the baseline migration would crash on a pre-Alembic
  deployment (tables already created by `create_all`, no `alembic_version`).
  Made the baseline idempotent (per-table `has_table` guard) so such a database
  adopts the baseline instead of failing. Verified against a simulated
  pre-existing database and a fresh one.
- **MEDIUM/LOW** (Vibe): friendlier entrypoint logging, more HTTP status codes
  in the error map, trailing newline in `alembic/README`.
- Declined (verified false positives): `str(database_url)` cast (already typed
  `str`), `uv run`/PATH in entrypoint (Dockerfile sets `PATH`), DB-readiness
  loop (compose `depends_on: service_healthy` guarantees it), module-level
  `get_settings()` (all settings have defaults).

### Phase 0 bench + local stack

Established this machine as the client integration/visual bench: installed the
Rust toolchain (Arch `rustup`), brought up the server stack locally in Docker
(`postgres + api + caddy`), and verified the GNOME (GTK4) client logs in
end-to-end against it. Installed and wired `pre-commit` (the gitleaks
secret-scan hook was configured but not actually active locally before this).
