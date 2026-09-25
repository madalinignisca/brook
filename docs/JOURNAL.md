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

## 2026-09-25

### Calls acceptance, macOS <-> Linux, overnight (test server)

All on the LAN test server (server `main` frozen for the runs; calls in the
`calltest` channel), with the headless Linux participant
(`clients/gst-media/examples/call_participant.rs`) against the macOS engine.

- **Audio + video, both ways (synthetic media):** Linux decoded the Mac's
  H.264 video at ~30 fps and its Opus audio in every run (e.g. 434-515 frames
  over 14-17 s), each ending with a clean leave. The Mac decoded the Linux
  H.264 (42e01f) via VideoToolbox at ~30 fps and played its audio.
- **Real Mac app (owner present):** Linux received the real camera and mic;
  mute and camera-off reached the Linux roster, camera-off really stopped the
  video (frame count frozen), and video resumed at full rate on camera-on.
- **Echo check (automated, built-in speakers + mic):** "no echo detected":
  peak envelope correlation 0.068 vs chance 0.044 (the positive control reads
  0.96); the received level rose ~19 dB while the clip played, uncorrelated
  with it (not audible echo; cause not isolated). A first run on a USB
  headset was discarded as meaningless for speaker echo.
- **Screen share, both ways (contract labels via `call.publish` `tracks`):**
  the Mac decoded the Linux share (source `screen`, ~10 fps) next to the Linux
  camera; Linux decoded the Mac's (`Video:Screen`, ~12 fps) and the stop
  re-offer removed it before the leave. The server side reported 6 DTLS
  completions, 0 Janus errors, 0 API warnings for the window.
- Found and fixed along the way: the server treated `m=... 0` + `a=bundle-only`
  (webrtcbin's BUNDLE shape) as inactive (fixed server-side); a fresh screen
  transceiver's kind read `unknown` right after the offer (fixed in the Linux
  engine); `republish` during a negotiation was refused (now queued in core);
  a republish error after a share toggle was dropped in the GNOME window.

Still open for the owner: the real Mac app <-> real GNOME app call with
video and screen share (each desktop's picker needs consent).

## 2026-09-24

### P4 calls: GNOME joins real calls through core, reviewed by Vibe

Chat header gets a call button ("Start a call" / "Join call (N)" from
`channel.call`; insensitive on archived channels). It opens a call window
driven by `BrookClient::join_call` + `GstEngine`: engine candidates ->
`local_candidate`, engine errors -> `engine_failed`, `SubscribeStreams` +
`CallState` roster -> tile names / tile removal, status banner for
Joining/Reconnecting/Ended(reason), mic/camera -> `set_media`, hang up / close
window -> `leave`. Core's awaited `leave()` fix verified: the back-to-back
live test now passes 3/3 (it failed every time before). **Not run on screen yet** (libadwaita
missing); the same engine + core path is proven headless by `core_call.rs`.

Friends review: only Vibe ran. Declined all three (engine.close() on window
close / close-during-join / engine error): core's MediaEngine contract calls
close() exactly once on every end path (leave, engine_failed, handle drop);
the UI closes it itself only when join fails, before a handle exists.

### P4 calls: live call through core's signaling (C1b acceptance on Linux)

`clients/gst-media/tests/core_call.rs` (ignored, live): two `BrookClient`s +
`GstEngine`s via `join_call`/`CallHandle` against local Janus. Passes: joined
-> Connected, rosters, video both ways, `set_media` reflected in the other
roster, leave -> Ended(Left) + empty re-offer applied. Found by running it
twice back-to-back: a ghost participant in the next call. Two causes, reported
to their owners: server `_leave` replies before removing the participant (a
failed send skips removal; ghost until the 30 s grace), and core's `leave()`
is fire-and-forget (spec said reply awaited), so an app exiting right after
leave races it.

### P4 calls: GstEngine implements core's MediaEngine, reviewed by Vibe

Branch `feat/gtk-calls` = `feat/gtk-media-engine` + the core side's
`feat/core-call-signaling` (P4 public types). `brook-media-gst` now uses core's
`PcKind`/`IceCandidate`/`IceServer`/`SubStream` (one definition) and implements
`brook_core::MediaEngine`. Contract obligations met and tested
(`tests/engine_contract.rs`): usable as `Arc<dyn MediaEngine>`; `close()`
fences the engine (later or in-flight operations fail and build nothing; the
fence tests fail with the fence disabled); `set_ice_servers` applies to PCs
built afterwards; new `EngineEvent::SubscribeStreams` hands the UI each applied
offer's mid -> participant map. Live Janus interop still passes. Vibe: nothing
material.

### P4 calls: webrtcbin <-> Janus interop proven (local stack)

Ran the server side's `feat/calls` (api + Janus 1.4.2 VideoRoom) in local docker
(`docker compose -p brookcalls --profile media`, separate project so the dev
stack's volumes are untouched) and a new ignored test,
`clients/gst-media/tests/janus_interop.rs`, with a minimal in-test signaling
client (not the product's: that is core's C1b). Result, two GStreamer engines
(alice, bob; synthetic H.264 + Opus): both decode each other's video through
Janus; bob leaving -> alice gets re-offer v2 with no streams, answer `call.ok`;
bob rejoining -> re-offer v3 on alice's SAME subscribe PC (Janus reuses mids
0/1), webrtcbin emits new remote pads and decoding resumes. Follow-up fix:
decode chains of superseded pads are now retired (per-mid chains, torn down
via `call_async` off the streaming thread); the interop test asserts one
decoder per active mid and fails without the fix. Vibe: nothing material.

### KDE: return to login on mid-session sign-out, reviewed by Vibe

Same requirement as GNOME (from the core side, before core's session-epoch fix merges):
a watcher on the core `AuthState` flips `logged_in` back on `LoggedOut`, so
Kirigami swaps the chat page for the login page; the shared client is cleared.
The chat realtime listener now uses a generation counter instead of a
one-shot `STARTED` flag (which blocked any restart after re-login), and it
exits on sign-out. Builds and clippy-clean; not exercised at runtime yet.

Friends review: only Vibe CLI 2.14 ran. Applied: the listener no longer idles
forever (holding the client) after a sign-out with no re-login. Declined:
"tasks outlive the Qt controllers" (both controllers live at the root of
Main.qml for the window's lifetime; `qt.queue` fails harmlessly after).

### GNOME login: remembered Server field + mid-session sign-out, reviewed by Vibe

Parity with macOS (user asked for a server field kept across restarts): login
form gets a Server row; the server of the last *successful* login is saved to
`$XDG_CONFIG_HOME/brook/gnome.ini` (GKeyFile, not GSettings: an uninstalled
`cargo run` has no compiled schema); `BROOK_SERVER` still overrides. A client
is created per server; its auth watcher stops once replaced. Requested by
the core side before core's session-epoch fix (C1b P1) merges: a mid-session
`LoggedOut` now tears the chat view down and returns to login; the chat
realtime loop stops once its view is gone (else a rebuilt view on the same
client double-handles events, e.g. duplicate notifications). **Not run on
screen yet** (libadwaita missing on the bench).

Friends review: only Vibe CLI 2.14 ran. Applied: rebuild chat after a
server switch (via the sign-out teardown), stop superseded auth watchers.
Declined: "curly quotes may render incorrectly" (UTF-8 in GTK is fine; the
codebase already uses them).

### P4 calls: GNOME call view + dev loopback, reviewed by Vibe

`clients/gnome/src/call.rs`: video tile grid (`gtk4paintablesink` ->
`gtk::Picture`), self-view overlay, mic/camera/hang-up controls, status banner;
`BROOK_CALL_LOOPBACK=1` runs a call with yourself through two engines (no
server). Logging now caps `tungstenite`/`tokio_tungstenite` at `info` even under
`RUST_LOG=trace` (they dump whole frames incl. tokens; verified in a scratch
program). **Not yet run on screen**: libadwaita is currently uninstalled on the
bench, so this is type-checked + clippy-clean only.

Friends review: only Vibe CLI 2.14 ran (Codex login expired, Gemini no API key).
Applied: preflight `gtk4paintablesink`/`autoaudiosink` instead of panicking on a
GStreamer thread; log failed loopback ICE adds. Declined: "publisher leaks when
the second engine fails" (engines build pipelines lazily, dropping the `Arc`
frees everything); "`RUST_LOG=error` gets raised to info for tungstenite"
(harmless: info carries no frames; the directive exists to cap trace).

### P4 calls: Linux media engine (`clients/gst-media`), reviewed by Vibe

Coordinated P4 with the server and core/Apple sides: wire
contract = PROTOCOL.md §3 (PR #10), signaling lives in `core` (C1b),
media is per client. New toolkit-free crate `brook-media-gst` on GStreamer
`webrtcbin`: publish PC (sendonly, client offers, H.264 advertised
`profile-level-id=42e01f` pmode=1 + Opus; VP8 fallback) and subscribe PC
(recvonly, SFU offers, renegotiable), trickle ICE both ways, mute/camera
toggles with keyframe on resume, app-injected sinks. In-process loopback test
(publish PC -> subscribe PC) passes for H.264+Opus and VP8+Opus.

Friends review: only Vibe CLI 2.14 ran (Codex login expired, Gemini had no
`GEMINI_API_KEY`). Applied: quote the V4L2 device path in the launch
description (regression test), propagate missing-element errors in the remote
decode chain instead of linking a shortened chain, warn when webrtcbin rejects
a TURN server, document the GStreamer build requirement. Declined: none.

## 2026-06-20

### PN typing indicators (slice B) — both clients, reviewed by Codex/Gemini/Vibe

Ephemeral, no DB: the composer fires a debounced (~1/3s) `POST /channels/{id}/typing`
which fans a `typing` WS event to the channel's other members; both clients show
"X is typing…" above the composer, auto-clearing after 4s and on channel switch.
core: `client.send_typing`, `ServerEvent::Typing`. 51 server + 9 core.

Applied review (Codex clean):
- HIGH (Gemini): `/typing` was an unthrottled DB query + O(N) fan-out per request —
  a client could bypass its debounce and spam it → added a **server-side throttle**
  (~1 per 2s per user+channel, short-circuits before the member query; bounded map).
- MED (Gemini): KDE typing label was `AutoText` → HTML in `display_name` injects →
  `Text.PlainText` (GNOME's label is plain by default).
- Declined (Vibe): `unwrap_or_default` on `me` "filters valid typing" — the server
  already excludes the sender, so an empty `me` filters nothing (his fix would drop
  typing when `me` is unresolved); "timeout callback nulls newer timeouts" — old
  timeouts are `remove()`d first, so stale callbacks never fire.

### PN @mentions (slice A) — both clients, reviewed by Codex/Gemini/Vibe

@handle / @channel / @here. Server resolves mentions **only on the live send**
(`message.new`) — `MessageOut.mentions` = specific member ids, `mention_everyone` =
@channel/@here flag. Clients: highlight `@name` in blue-bold (outside code spans),
and a distinct "X mentioned you" notification when `mention_everyone || you ∈ mentions`.
51 server + 9 core tests.

Applied review (Codex/Gemini/Vibe — several architectural catches):
- HIGH (Gemini): resolving mentions per **history** read mis-resolves against
  today's membership + is N+1 → make mentions **send-only** (history/edit/search
  don't recompute; clients highlight from the body text anyway). Removed code.
- HIGH (Gemini): `@channel` expanding to all member ids = huge payload → a
  `mention_everyone` bool instead.
- MED (Codex): handles are case-sensitive-unique → match `@handle` case-sensitively
  (keep `@channel`/`@here` case-insensitive) so `Alice`/`alice` can't cross-notify.
- MED (Codex/Vibe): `\w+` missed `.`/`-` handles (`@jane-doe`) → handle-charset
  regex (excludes trailing punctuation) + widened client highlighters.
- Declined: KDE "missing headings/blockquotes/lists" — the custom safe-HTML emitter
  now matches GNOME's same subset (parity is the goal; text still renders). Deferred
  (LOW): `@` inside a code block still notifies server-side; Unicode handles (handles
  are ASCII-only).

### Markdown & code blocks — both clients, reviewed by Codex/Gemini/Vibe

Render-only markdown (bold/italic/inline-code/code-block/link/strikethrough) — no
server/core change (body stays raw text). GNOME: `pulldown-cmark` → Pango markup
(text escaped, raw HTML dropped); KDE: a `render_markdown` invokable converts to a
**safe HTML subset** (raw HTML + images dropped) rendered via `Text.RichText` —
parity with GNOME rather than Qt's `Text.MarkdownText` (which would pass through raw
HTML). Links open externally.

Applied three-model review (Codex/Gemini/Vibe — strong consensus on link safety):
- HIGH (Gemini ×2, Vibe ×2, Codex MED): link clicks ran arbitrary URI schemes
  (`file:`, `smb:`, `javascript:`) → allowlist `http`/`https`/`mailto` on both.
- MED (Codex): KDE `Text.MarkdownText` rendered untrusted raw HTML/images → render
  a sanitized HTML subset in Rust (drop HTML + images) instead.
- LOW (Codex): GNOME's `pango::parse_markup` guard rejected `<a href>` (a GTK-label
  extension, not core Pango), so every link fell back to plain text → removed it.
- MED (Gemini): paragraph newline-at-start broke list items → newline at paragraph
  *end*. Declined: "markdown is a breaking change" (rendering only; protocol
  unchanged — noted here); strip `.trim()` (markdown normalizes whitespace).

Note: rendering is a client display change only — `message.body` is still stored
and transmitted as raw text. Image-proxy/remote-resource hardening is a P2 item.

## 2026-06-19

### P1b channel management + public self-join — both clients (slices B/C), reviewed by Codex/Gemini/Vibe

A channel-settings menu (Rename / Archive·Unarchive / Delete) shown to admins on
real channels; archived channels disable the composer; `channel.delete` clears the
open view. Browse-public dialog (self-join) + a "public" toggle on channel create.
core gained `client.is_admin()`; KDE exposes it as an `admin` qproperty to gate the
UI (GNOME already had `is_admin`).

Applied review (Codex/Gemini/Vibe):
- MED (Codex, both clients): archiving the OPEN channel left the composer/header
  stale until reselect → re-sync current-channel chrome on reload (GNOME
  `apply_channel_chrome` extracted + called from `refresh_channels`; KDE re-syncs
  `currentKind`/`currentArchived` in `onChannels_loaded`).
- Declined (Gemini HIGH ×2): "`Some(&name)` won't compile" and "`AdwAlertDialog`
  has no `.close()`" — both false; the code builds clean (`&String`→`&str` coerces
  under the expected type; `AdwDialog::close()` exists in adw 1.5).
- Declined (Vibe): magic "admin" string + silent create errors — consistent with
  existing literal / `is_ok` patterns; `select_channel` not-found — selection
  always maps to a listed channel.

### Fix — KDE buttons invisible on non-Plasma themes (reactions, search, settings)

User on GNOME: KDE reply/edit/delete worked but **reactions were missing**. Root
cause: the reaction picker was an `IconOnly` ToolButton with `icon.name:
"smiley-add"` — a Breeze name absent from Adwaita, so it rendered as a zero-size
empty button (and with no existing reactions, no chips either). Verified against the
theme: `smiley-add`, `search`, `configure`, `dialog-close` are all MISSING in
Adwaita. Fix: the react button is now a visible "🙂 React" text label; the other
three Breeze-only names swapped to freedesktop-standard ones present in BOTH Breeze
and Adwaita (`edit-find`, `emblem-system`, `window-close`) — fixing the search,
channel-settings, and reply-cancel buttons too.

### P1b message search — both clients (slices B/C), reviewed by Codex/Gemini/Vibe

A search button → a dialog: type a term, get matching messages (channel · author:
body), click one to jump to its channel. GNOME: `AdwAlertDialog` + `SearchEntry`;
KDE: an `OverlaySheet` + `search` invokable / `search_results_loaded` signal.

Applied review (Codex/Gemini/Vibe):
- HIGH (Gemini): GNOME search-dialog closures formed a reference cycle that leaked
  the dialog tree on every open → weak `dialog` (downgrade/upgrade) in the entry +
  result-button closures.
- MED (Codex, Gemini): concurrent searches could resolve out of order and show
  stale results → a generation guard drops superseded responses (GNOME).
- LOW (Gemini, Vibe): KDE serialize fallback `"[]"` (not `""`) so QML `JSON.parse`
  never throws.
- Deferred (Codex/Gemini MED): clicking a result opens the channel but doesn't yet
  scroll to the specific message (needs load-around-id). Declined: try-catch around
  the now-safe JSON.parse, clear-before-await, zero-match label, KDE search race
  (consistent with existing patterns / cosmetic).

**GOAL "full #1" COMPLETE** — all 6 P1b features (edit/delete, quote-reply,
reactions, channel rename/archive/delete, public self-join, message search) shipped
full-stack across server + core + both clients, each friends-reviewed.

### P1b message search — server + core (slice A)

`GET /channels/search?q=` searches message bodies across the caller's channels
(case-insensitive ILIKE, LIKE wildcards in the term escaped; non-deleted;
newest-first; membership-scoped via a join). Postgres FTS is the noted P2 upgrade.
core: `client.search_messages(q) -> Vec<Message>`. Tests: finds matches
case-insensitively; results scoped to membership (a non-member can't find a private
channel's messages). 48 server + 9 core. Client search UI follows.

### P1b channel management + public self-join — server + core (slice A)

One migration (d4e5f6a7b8c9) adds `channels.public` + `archived_at` (features 4 & 5
share the table). Channel management (admin or owner; never DMs): `PATCH /channels/{id}`
(rename/topic/archive → `channel.update`), `DELETE /channels/{id}` (cascade →
`channel.delete`); sending to an archived channel is 403. Public self-join:
`POST /channels` takes `public`; `GET /channels/public` browses public non-archived
channels you haven't joined; `POST /channels/{id}/join` self-joins (→ `channel.update`).
core: `Channel.public`/`archived`, `ServerEvent::ChannelDelete`, client
`update_channel`/`delete_channel`/`create_public_channel`/`list_public_channels`/`join_channel`.
Tests: rename/archive-blocks-send/unarchive, non-owner 403, delete, public
browse+join, can't-join-private 404. 46 server + 9 core. Client UI follows.

### P1b emoji reactions — both clients (slices B/C), reviewed by Codex/Gemini/Vibe

Reaction chips (emoji + count, highlighted when `me`) under each message + a
per-message quick-react picker (👍❤️😂🎉👀🙏), all synced live via the incremental
`reaction.update`. GNOME: chips in a `reactions_box` per `MessageWidgets`, rebuilt
on each change; `apply_reaction` folds the increment into tracked tallies. KDE:
`toggle_reaction` invokable + `reaction_updated` signal; chips via a Repeater +
quick-react Menu.

Applied review (Codex/Gemini/Vibe — Gemini clean):
- MED (Codex): KDE — a nested array in a `ListModel` role becomes a nested
  ListModel (breaks `modelData`/`length`); store reactions as a **JSON-string role**
  and parse at use.
- HIGH (Vibe): GNOME `apply_reaction` double-borrow → clone the widgets once;
  `render_reactions` used `chat.current` → store/use `MessageWidgets.channel_id`.
- Declined (Vibe): "ReactionUpdate ignores channel_id" — `message_rows` is
  channel-scoped and ids are unique, so a cross-channel reaction matches no row.

### P1b emoji reactions — server + core (slice A)

`reactions` table (migration c3d4e5f6a7b8): composite PK (message_id, user_id,
emoji), both FKs `ON DELETE CASCADE`. `POST /messages/{id}/reactions {emoji}`
**toggles** the caller's reaction. `MessageOut.reactions` = per-emoji tally with the
caller's `me` flag (batch-aggregated in history via GROUP BY + `max(case...)`, no
N+1). Because `me` is per-recipient, the WS `reaction.update` carries an
*incremental* change `{message_id, channel_id, emoji, user_id, added, count}`, not a
full summary — each client adjusts its own view. core: `Message.reactions`,
`ReactionSummary`, `ServerEvent::ReactionUpdate`, `client.toggle_reaction`. Tests:
toggle/aggregate/`me` flag, non-member 404, WS fan-out. 41 server + 9 core. Client
reaction UI follows (slices B/C).

### P1b quote-reply — both clients (slices B/C), reviewed by Codex/Gemini/Vibe

A **Reply** action on any message → a banner above the composer ("Replying to X")
with cancel → send carries `reply_to_id`; the quoted excerpt renders inline above
the reply ("↳ author: …"). GNOME: a `Revealer` reply bar + `replying_to` state,
Reply added to the per-message `⋯` menu (now on every message). KDE: page
`replyingTo`/`replyingToText` + banner, Reply ToolButton in the delegate, quote
label; `send` invokable gained `reply_to_id`. Reply mode clears on channel switch.

Applied review (Codex/Gemini/Vibe — Vibe clean):
- LOW (Codex): a pending reply to a since-deleted message would lose the draft on
  send → clear the reply when its target is deleted (both clients).
- Deferred (Codex MED): live-updating an inline quote when the quoted message is
  later edited/deleted (the excerpt is point-in-time; needs per-quote tracking).
- Declined (Gemini HIGH): "reference cycle in the new closures" — identical to the
  file's ~22 other `Rc<Chat>` closures, already the documented Phase 1b
  weak-capture TODO; converting 2 of 22 wouldn't break the cycle.

### P1b quote-reply — server + core (slice A)

`messages.reply_to_id` (migration b7c2f1a9d3e4, self-FK `ON DELETE SET NULL` so a
reply survives the quoted message). `POST /messages` takes `reply_to_id` (validated
live + same channel via `_get_message`); `MessageOut` carries `reply_to_id` + a
truncated `reply_to` excerpt (author + 140-char body), batch-resolved in history to
avoid N+1, and on edit. core: `Message.reply_to_id`/`reply_to: ReplyExcerpt`,
`client.send_message(..., reply_to_id)`. 15 channel tests (excerpt in send + history;
foreign reply 404). Client reply UI follows (slices B/C).

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
