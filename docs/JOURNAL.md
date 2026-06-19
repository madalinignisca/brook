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

## 2026-06-19

### P1b edit/delete messages — both clients (slices B/C), reviewed by Codex/Gemini/Vibe

Per-message author-only **Edit** (prefilled dialog) + **Delete** (confirm), an
"edited" marker, all reflected live via `message.update`/`message.delete`. GNOME:
a `⋯` popover per own message + `AdwAlertDialog` (bumped adw feature to `v1_5`),
rows tracked by id in a `HashMap` for in-place update/remove. KDE: ToolButtons in
the delegate + `Kirigami.PromptDialog`s, model rows carry `mid`/`authorId`/`edited`.

Applied review (Codex/Gemini/Vibe):
- HIGH/MED (Gemini, Codex): the Edit action captured the original body, so
  reopening after a live edit reverted it → read the current label at click time.
- MED (Codex): duplicate ids could overwrite the row map (history + WS echo
  overlap) → de-dupe the old row on append.
- MED (Vibe): log edit/delete call errors.
- Declined: "remove `.await` after `runtime.spawn`" (false positive — cooperative
  inside `spawn_future_local`, same as `send_current`); channel_id check on
  update/delete (map is already channel-scoped); KDE `unwrap_or_default`
  (consistent, can't fail). Deferred (LOW): multi-line edit (composer is
  single-line too); admin-delete-others in the UI (a later moderation slice).

### P1b edit/delete messages — server + core (slice A)

`PATCH /channels/{id}/messages/{mid}` (author-only edit → `edited_at`, fans
`message.update`) and `DELETE …/{mid}` (author or global admin → soft-delete via
`deleted_at`, fans `message.delete` `{id,channel_id}`). No migration — the columns
already existed. Core: `Message.edited_at`, `ServerEvent::MessageUpdate` /
`MessageDelete`, `client.edit_message`/`delete_message`. Tests: 4 REST (author
edits, non-author 403, author deletes + 404-on-deleted, admin-deletes-others vs
member 403) + WS update/delete fan-out. 37 server + 9 core green. Clients next
(slices B/C). Not yet friends-reviewed — bundling with the client UI.

## 2026-06-18

### Desktop notifications don't render on GNOME Shell in dev — root-caused, deferred to packaging

After the server fix, unread works end-to-end but notifications show in **neither**
client on GNOME Shell. Root-caused: `notify-rust`/`gio` both report the send as
**Ok** (a real `message.new` notification fires, identity gating correct), and a
**standalone** notify-rust process **does** display on this same GNOME — but
neither client does. The distinguishing factor: GNOME Shell associates a *windowed*
app's notifications with its **app-id** and suppresses them unless a matching
`.desktop` is **installed and in the shell's cache**. The standalone has no window
(anonymous → shown); both clients have windows with app-ids GNOME can't resolve:
GNOME client = `dev.brook.Brook` (entry installed mid-session but shell not
re-read), KDE client = `brook-kde` (no entry, and it sets no `desktopFileName`).

Verified NOT the cause: tokio `spawn_blocking` vs std thread, the `-1` timeout,
identity gating. Code is correct and sends. **Deferred to packaging** (the part of
the stack that installs desktop entries + an icon, after which the shell has them).
Unread badges cover the in-app "new messages" signal meanwhile.

Packaging TODO: install `dev.brook.Brook.desktop` (GNOME) + a `dev.brook.kde`
entry, set `QGuiApplication::setDesktopFileName("dev.brook.kde")` on the KDE client,
ship icons; then verify notifications render (incl. a one-time shell refresh).

### Fix — read-state migration `max(uuid)` fails on Postgres (deploy-discovered)

Live testing: KDE spammed `mark_read failed: not_found`. Root cause was **not**
client code — the running server (uvicorn in Docker, up 40h) predated the unread
work: its OpenAPI had no `/read` route and no `unread_count` (the badges only
*looked* live because clients increment locally). Rebuilding the api container to
deploy current code then surfaced the real bug: migration `288a47108837`'s backfill
used `SELECT max(messages.id)` — **Postgres has no `max(uuid)` aggregate**, so the
migration crashed on startup (502). It passed CI because `test_migrations` runs on
SQLite, where `max` accepts any type — a real SQLite-vs-Postgres test gap.

Fix: newest id via `ORDER BY messages.id DESC LIMIT 1` (UUIDv7 is time-sortable;
works on both engines). Redeployed: migration applies, `/read` → 204, `unread_count`
served. TODO: exercise migrations against Postgres in CI to close the gap.

### #3 fix — GNOME notifications via gio (not freedesktop), reviewed by Codex/Gemini/Vibe

Live testing: GNOME showed no notifications. Diagnosed (notify-rust `show()`
returned Ok, app registered as a GApplication `dev.brook.Brook`, no desktop entry,
not in GNOME's notification app list) → **GNOME Shell drops raw freedesktop
`Notify` from a registered GApplication**; it renders only GTK notifications
(`org.gtk.Notifications`) tied to an installed `.desktop`. Switched the GNOME
client to `gio::Notification` via the app, added `data/dev.brook.Brook.desktop`
(+ install/README note); KDE keeps notify-rust (Plasma renders it). Explicit 5s
timeout on both (notify-rust default `-1` = persistent banner that blocks others).

Caveat: GNOME only displays once the desktop entry is installed AND the shell has
re-read it (re-login / packaging). The notification *logic* is verified correct.

Applied review (Codex/Gemini/Vibe): document/install the `.desktop` (Codex);
warn when no default GApplication (Vibe). Declined: notify-rust GNOME fallback
(GNOME drops it), `send_notification` error log (returns no Result).

### "Make it alive" #3 — desktop notifications, reviewed by Codex/Gemini/Vibe

Native desktop notifications via freedesktop D-Bus (`notify-rust`) — works on
GNOME and Plasma without an installed `.desktop` file (native gio/KNotifications
is later polish). Fires for a message in a channel you're **not** viewing,
skipping your own; shown off the UI thread (`spawn_blocking`). GNOME notifies
inline in the event loop; KDE exposes a `notify()` invokable the QML calls.

Applied three-model review (Codex: clean; Gemini; Vibe on retry):
- HIGH (Vibe): own-message notifications could fire when identity isn't resolved
  (`unwrap_or_default`/empty `my_id`) → only notify when identity is known *and*
  differs.
- MEDIUM (Vibe): log `Notification::show()` errors instead of swallowing.
- Declined (Gemini): "notifies for the active channel" — verified false; the
  notify call is already inside the non-current-channel branch.

### "Make it alive" #2b — unread badges in both clients, reviewed by Codex/Gemini/Vibe

Sidebar unread badges fed by `unread_count`: GNOME renders a per-row count label
(parallel `badges` vec), KDE an `unread` model role + delegate badge. Both bump
live on a message in a non-open channel, and clear + `mark_read` on open. Marks
read for messages received while the channel is open, too.

Applied three-model review (Codex gpt-5.5, Gemini CLI, Vibe):
- HIGH (Codex, Gemini): a RefCell panic in GNOME `select_channel` — an inline
  `if let chat.channels.borrow()…` held the immutable borrow into the body where
  `borrow_mut()` runs → panic on every channel select. Extract idx first.
- MEDIUM (Codex): messages received while viewing a channel weren't marked read
  server-side → mark-read on current-channel messages (both clients).
- HIGH/MED (Vibe): KDE `unread_count || 0` guard; log `mark_read` errors.

### "Make it alive" #2a — unread read-state (server + core), reviewed by Codex/Gemini/Vibe

Foundation for unread badges/notifications: `memberships.last_read_message_id`
(migration 288a47108837), `ChannelOut.unread_count` (single GROUP BY query),
`POST /channels/{id}/read` (mark up to a message or the latest), author
auto-reads their own sends. Core: `Channel.unread_count` + `client.mark_read()`.
33 server + 9 core tests. (Client badge rendering is the next increment.)

Applied three-model review (Codex gpt-5.5, Gemini CLI, Vibe):
- HIGH (Gemini): `last_read=None` counted ALL history as unread → flood. New
  members now start at the channel's latest message; a migration backfill catches
  existing members up.
- MEDIUM (Codex/Gemini/Vibe): `mark_read` accepted a forged/foreign `message_id`
  (could silence unread forever) → validate it exists in the channel.
- MEDIUM (Gemini): `send_message` could rewind the read cursor → advance-only.
- MEDIUM (Gemini/Vibe): N+1 unread query → single GROUP BY.

### "Make it alive" #1 — live channel updates + add-member UI, reviewed by Codex/Gemini/Vibe

Closes the "added to a channel but had no idea" gap. Server emits `channel.update`
to a channel's members on `add_member` and on DM creation (the other member);
core relays it as `ServerEvent::ChannelUpdate`; both clients reload their channel
list live on receipt. Add-member UI: a header popover (GNOME) and a toolbar
button + sheet (KDE), wired to `add_member`. Server test added (30 total).

Applied three-model review (Codex gpt-5.5, Gemini CLI; Vibe: nothing material):
- MEDIUM (Codex): made `ServerEvent` `#[non_exhaustive]` (+ catch-all arms) so
  future event kinds don't break consumers.
- MEDIUM (Gemini): KDE awaited the channel reload inside the WS loop (could lag/
  drop events) → now spawned detached.

### Phase 1 fix — session token refresh + WS live-token, reviewed by Codex/Gemini/Vibe

First live two-account testing exposed: after the 15-min access token expired,
sends returned `auth.invalid_token` and the realtime socket couldn't recover (it
held a static token). Fix in `core`:

- A background loop refreshes the access token (~10 min, before the 15-min TTL)
  via `/auth/refresh`, rotating both tokens in the shared session.
- The WebSocket reads the *current* token from the shared session on each
  (re)connect; both clients now connect + subscribe (verified in logs).
- KDE gained a `tracing` subscriber so core logs (incl. WS lifecycle) surface.

Applied three-model review (Codex gpt-5.5, Gemini CLI, Vibe):
- HIGH (Codex, Vibe): refresh TOCTOU — an in-flight refresh could write old
  tokens into a newer (relogin) session. Compare-and-set on the refresh token.
- HIGH/MED (Vibe, Gemini): a rejected refresh token (4xx) retried forever. Now
  clears the session and stops; transient errors retry in 15s, not 10 min.
- HIGH/MED (Gemini, Codex): loops exited on logout without resetting the guard,
  blocking relogin. Loops now idle-and-poll for a session and self-heal.
- LOW (Gemini): KDE error logging moved from eprintln to tracing.

### Phase 1 KDE client — chat UI (Kirigami + CXX-Qt), reviewed by Codex/Gemini/Vibe

`clients/kde`: a `ChatController` CXX-Qt bridge (`src/chat.rs`) exposing
start/refresh/select_channel/send/open_dm/create_channel and emitting JSON to QML
via `channels_loaded`/`history_loaded`/`message_received` signals; `qml/Main.qml`
gains a two-pane chat page (channel list + messages + composer + new-conversation
sheet). `src/app.rs` shares one authenticated `BrookClient` between the login and
chat controllers. Core chat DTOs gained `Serialize` for the JSON bridge.

Applied three-model review (Codex gpt-5.5, Gemini CLI):
- **MEDIUM** (Gemini): subscribed to `events()` *after* `start_realtime()`, racing
  missed events. Subscribe first now.
- **LOW** (Gemini): guarded the realtime listener with a once-flag so a recreated
  chat page can't spawn overlapping loops.
- **LOW** (Codex): clear the message model on channel switch (don't show the old
  channel while loading).
- **MEDIUM** (Vibe): null-guard `members` in the QML title helper.
- **Declined** Gemini's HIGH ("signals are camelCase in QML") — cxx-qt 0.8 keeps
  snake_case (proven when `logIn` failed and `log_in` worked at login; the moc
  header shows `Q_SIGNAL channels_loaded`), so `onChannels_loaded` is correct.
- Deferred: optimistic send + WS-echo dedupe (same as GNOME; WS normally up);
  surfacing network errors to the UI.

### Phase 1 GNOME client — chat UI, reviewed by Codex/Gemini/Vibe

`clients/gnome/src/chat.rs`: an `AdwOverlaySplitView` with a channel/DM sidebar,
message list, and composer over `brook-core`. Networking on the Tokio runtime
(await `JoinHandle` on the GLib loop); realtime `message.new` events consumed
from the core broadcast channel and appended live. A "+" popover opens DMs by
handle and (admins) creates channels. `main.rs` swaps the login placeholder for
the chat view on sign-in.

Applied three-model review (Codex gpt-5.5, Gemini CLI):
- **HIGH** (Codex, Gemini): `start_realtime().await` ran on the GLib executor but
  spawns a Tokio task → would **panic at login**. Now run via `runtime.spawn`.
- **MEDIUM** (Codex): history load cleared the list *after* the await, wiping
  live messages that arrived meanwhile. Clear *before* the await now.
- Deferred (TODO): strong `Rc<Chat>` capture cycle (nil impact for the
  single-window session; weak-capture refactor later). Vibe produced no findings.

### Phase 1 core — chat client (channels, messages, realtime WS), reviewed by Codex/Gemini/Vibe

(See commit 970ef92.)

### Phase 1 server — chat (channels, DMs, messages, WebSocket fan-out), reviewed by Codex/Gemini/Vibe

The server half of Phase 1: two accounts can chat 1:1 and in channels.

- Models + migration (`3e79d72bf1f9`): `channels` (dm|channel), `memberships`
  (owner|member), `messages` (UUIDv7 id → time-sortable pagination, soft-delete).
- REST (`routers/channels.py`): `GET/POST /channels` (channel = **admin only**;
  dm = find-or-create by member handle), `POST /channels/{id}/members`,
  `GET /channels/{id}/messages` (before/after pagination, oldest→newest),
  `POST /channels/{id}/messages` (single send path → persist → fan out).
- Realtime: in-process hub (`app/hub.py`) + `/ws` (auth in first frame, replies
  `ready`, then `message.new` envelopes). Single-node; scale-out deferred.
- Tests: 29 total incl. a live WS test (alice→bob delivery) and REST coverage
  (perms, find-or-create DM, pagination). Verified on Postgres.

Applied three-model review (Vibe, Codex gpt-5.5, Gemini CLI):
- **HIGH** (Gemini, Vibe): `channels.created_by` was non-nullable but FK is
  `ondelete=SET NULL` → IntegrityError on creator deletion. Made it nullable
  (model + schema + migration).
- **MEDIUM** (Codex, Gemini): `add_member` 404'd a global admin who wasn't a
  channel member, making the admin-override dead code. Now authorizes without
  requiring caller membership.
- **MEDIUM** (Gemini): history used an inner join on the FK-less `author_id`,
  silently dropping messages from bots/deleted users. Switched to outerjoin.
- **MEDIUM/LOW** (Vibe): narrowed the WS auth exception catch and dropped the
  loose `token` alias (accept only `access_token`).
- Deferred: DM find-or-create race under concurrency (TODO: canonical per-pair
  key) — no concurrency in Phase 1 manual testing.

### KDE/Plasma client — Phase 0 login skeleton (Qt6 + Kirigami via CXX-Qt)

Second Linux client, peer to GNOME, which also proves the Qt<->Rust binding
before the macOS/Windows FFI clients. `clients/kde`: a Kirigami login page over
`brook-core` via **CXX-Qt 0.8**, mirroring the GNOME reactive flow.

- `src/login.rs`: a `LoginController` QObject (CXX-Qt bridge) exposing
  `logIn(server, handle, password)` and `busy`/`loggedIn`/`errorText`/
  `displayName` properties. Networking runs on a Tokio runtime; results are
  marshalled back onto the Qt thread via `cxx_qt::Threading`.
- `qml/Main.qml`: Kirigami `ApplicationWindow` (follows Plasma theme/accent),
  login form -> home placeholder, with the dev plain-http opt-in
  (`BROOK_ALLOW_INSECURE_HTTP`) honored like the GNOME client.
- Builds (CXX-Qt links Qt statically) and runs; the Kirigami window loads clean.
  fmt + clippy clean. Login verified end-to-end against the local stack.

Friends review (Codex gpt-5.5, Gemini CLI, Vibe) — applied:
- **MEDIUM** (Codex): adding `clients/kde` to workspace `members` made
  `cargo … --workspace` (and CI) try to build it, which needs Qt6/CXX-Qt/Kirigami
  CI doesn't have. Added `default-members = [core, gnome]` and dropped
  `--workspace` from the rust CI so default builds skip kde (build it with `-p`).
- **LOW** (Codex): untracked `clients/kde/.qmlls.ini` (machine-specific absolute
  path from the QML language server) and git-ignored it.
- Declined: Codex's "LoginController may be uncreatable" — `#[derive(Default)]`
  supplies the constructor (verified at runtime); Gemini's `initialPage` won't
  transition — it does in this Kirigami version (verified). Vibe ran in plan mode
  and gave only categories; its concerns mirror the existing GNOME client.

Tooling note: also wired a machine-global `friends-review` command
(`~/.local/bin` + a `/friends-review` Claude command) so the three-model review
runs the same way in every project.

## 2026-06-17

### Test hardening from the friends reviews

Turned the friends' findings into regression tests and tightened weak ones
(15 → 21 tests):

- `tests/test_migrations.py`: the pre-Alembic **adoption** case (the HIGH
  finding) + fresh install + downgrade/upgrade round-trip.
- `tests/test_errors.py`: error-envelope contract — bare 404, 405, and an
  **unhandled 500** (regression for the catch-all handler finding).
- `tests/test_auth.py`: replaced loose assertions (`status in (401, 403)` — the
  kind of slack that *hid* the original 403-vs-401 bug) with exact error codes.

Both regression tests were verified to **fail without their fix** (guard removed
-> "table already exists"; handler removed -> non-JSON 500 body), then pass with
it restored — so they actually bite.

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
