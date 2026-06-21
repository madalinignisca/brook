# Feature plan

> The **detailed, tracked** feature catalog. [ROADMAP.md](ROADMAP.md) is the
> phase overview (the "when"); this file is the "what", item by item, with
> status. Scoped against Slack / Microsoft Teams / Mattermost **core** features —
> deliberately filtered through Brook's philosophy.

## Guiding line (what "core, never heavy" means here)

Brook ships the **conversation core** that a small self-hosted team needs, with
**native, resource-respecting** clients. When a feature is "table stakes for team
chat" it's in; when it's a platform/growth/enterprise-suite feature, it's out
(see [§Out of scope](#out-of-scope)). Single-workspace per server. No E2EE
([SECURITY.md](SECURITY.md)). When in doubt: the lighter, simpler option wins.

## Status legend

- ✅ done · 🟡 partial · ⬜ planned · ❓ open decision (see [§Open decisions](#open-decisions))
- Phase tags map to [ROADMAP.md](ROADMAP.md): **P0/P0b** auth, **P1** chat,
  **P2** files, **P3** bots, **P4** calls, **P7** mobile/push. **PN** = a new
  workstream proposed below (Notifications, Presence).

---

## 1. Identity & accounts

| Feature | Status | Phase | Notes |
|---|---|---|---|
| Local password login | ✅ | P0 | |
| Refresh-token rotation + auto-refresh | ✅ | P1 | client refreshes before expiry |
| TOTP, OIDC (Keycloak), LDAP | ⬜ | P0b | see [AUTH.md](AUTH.md) |
| Profile: display name | ✅ | P0 | |
| Profile: avatar | ⬜ | P2 | needs file storage; `users.avatar_file_id` |
| Profile: status message + emoji ("In a meeting") | ⬜ | PN | lightweight, drives presence UI |
| Account deactivate (keep history) | ⬜ | P7 | `users.status=deactivated` exists |
| Multi-device sessions + revoke | 🟡 | P0b | refresh tokens exist; no session list UI |
| Logout | ✅ | P0 | server revoke done; client wiring ⬜ |

## 2. Conversations & channels

| Feature | Status | Phase | Notes |
|---|---|---|---|
| 1:1 DM (find-or-create) | ✅ | P1 | |
| Group DM (multi-person, unnamed) | ⬜ out | — | **decided out** — use a small private channel instead |
| Private channel (invite-only) | ✅ | P1 | current default |
| Public channel (browse + self-join) | 🟢 | P1b | `public` flag + `GET /channels/public` + `POST /join`; browse dialog + create toggle in both clients |
| Channel topic / description | 🟡 | P1 | topic stored; no edit UI |
| Create channel (admin) | ✅ | P1 | |
| Add member (invite) | ✅ | P1b | API + UI in both clients + live `channel.update` |
| Remove member / leave channel | ⬜ | P1b | |
| Rename channel | 🟢 | P1b | `PATCH /channels/{id}` + `channel.update`; settings menu in both clients |
| Archive channel | 🟢 | P1b | `archived_at`; read-only + composer disabled; settings menu in both clients |
| Delete channel | 🟢 | P1b | `DELETE /channels/{id}` (admin/owner) + `channel.delete`; settings menu in both clients |
| Channel roles (owner / member) | 🟡 | P1 | stored; only used for add-member authz |
| Default / auto-join channels | ⬜ | P1b | e.g. everyone joins #general |

## 3. Messaging

| Feature | Status | Phase | Notes |
|---|---|---|---|
| Send message (single path) | ✅ | P1 | REST → WS fan-out |
| History pagination (`before`) | ✅ | P1 | `after`/forward-sync ⬜ |
| Edit message | 🟢 | P1b | `PATCH /messages/{id}` (author-only) + `message.update`; UI in both clients |
| Delete message (soft) | 🟢 | P1b | `DELETE /messages/{id}` (author/admin) + `message.delete`; UI in both clients |
| Markdown formatting (bold/italic/code/links) | 🟢 | P1b | render: GNOME pulldown-cmark→Pango, KDE `Text.MarkdownText`; clickable links |
| Code blocks | 🟢 | P1b | rendered (monospace/block) in both clients |
| Mentions (@user, @channel, @here) | 🟢 | PN | server-resolved `MessageOut.mentions`; @name highlighted + 'mentioned you' notification (both clients). Mention badge counts ⬜ |
| Emoji reactions | 🟢 | P1b | `reactions` table + toggle endpoint; chips + quick-react picker in both clients, live via `reaction.update` |
| Quote-reply (inline, no thread panes) | 🟢 | P1b | `reply_to_id` + resolved excerpt; Reply action, composer banner, inline quote in both clients |
| Pinned messages | ⬜ | P1b | |
| Link to message / copy link | ⬜ | P1b | |
| Unread / read-state tracking | 🟡 | PN | server+core done (read marker, unread_count, mark-read); client badges ⬜ |
| Drafts (per channel) | ⬜ | P1b | client-local |
| Message search | 🟢 | P1b | `GET /channels/search?q=` (ILIKE, membership-scoped); search dialog in both clients. Postgres FTS = P2 upgrade |
| File attachments | ⬜ | P2 | |

## 4. Presence & realtime (PN — Presence workstream)

| Feature | Status | Phase | Notes |
|---|---|---|---|
| Presence: online / away / offline | ⬜ | PN | ephemeral (in-memory), `presence.update` event |
| Typing indicators | 🟢 | PN | `POST /channels/{id}/typing` fans an ephemeral `typing` WS event; debounced send + 'X is typing…' in both clients |
| **Live channel updates** (added/removed/renamed/archived) | 🟢 | PN | added/renamed/archived via `channel.update`; deleted via `channel.delete` |
| Live membership in a channel | ⬜ | PN | |
| Reconnect forward-sync (`after=`) of missed messages | 🟡 | PN | WS reconnects; client doesn't yet replay misses |

## 5. Notifications & event feedback (PN — the priority workstream)

> Today an event like "you were added to #general" produces **zero feedback**.
> This workstream makes user-relevant events **visible**. Built on read-state (§3)
> and realtime events (§4).

### In-app

| Feature | Status | Phase | Notes |
|---|---|---|---|
| Per-channel unread indicator (bold/dot in sidebar) | ⬜ | PN | |
| Unread + mention badge counts (per channel + total) | ⬜ | PN | mentions count separately/stronger |
| Mark-as-read (on view) + mark-all-read | ⬜ | PN | |
| Activity / notifications feed ("added to #x", "@you in #y") | ⬜ | PN | a dedicated surface for user events |
| In-app toast/banner for live events | ⬜ | PN | |

### Native (per-OS)

| Feature | Status | Phase | Notes |
|---|---|---|---|
| Desktop OS notification | 🟡 | PN | code sends (GNOME gio / KDE notify-rust); GNOME Shell renders only with an installed+cached `.desktop` per app-id → **verify at packaging** (see JOURNAL) |
| Tray icon w/ unread count | ⬜ | PN | KDE `KStatusNotifierItem`; GNOME via extension/portal |
| Notification sound | ⬜ | PN | |
| Mobile push (messages, mentions, call invites) | ⬜ | P7 | APNs/FCM, per [PROTOCOL.md](PROTOCOL.md) §3a |

### Delivery preferences

| Feature | Status | Phase | Notes |
|---|---|---|---|
| Per-channel level: all / mentions only / mute | ⬜ | PN | |
| Global Do-Not-Disturb / snooze | ⬜ | PN | |
| Desktop-vs-mobile routing (don't double-notify) | ⬜ | P7 | per-device, see PROTOCOL §3a |

**Events that should notify / give feedback:** new message (per pref), @mention,
DM received, added to / removed from a channel, channel renamed/archived,
incoming call. This is the concrete checklist PN must cover.

## 6. Files & media (P2)

Upload/download via presigned URLs (`pending`→`committed`), inline image
previews + thumbnails, drag-drop / paste, per-channel file list. ⬜

## 7. Calls (P4)

1:1 + group video, screen share, mute/camera toggle, incoming-call UI. ⬜
See [MEDIA.md](MEDIA.md).

## 8. Bots & integrations (P3)

Inbound + outbound webhooks (HMAC, SSRF-guarded), `/slash` commands, bot as a
first-class channel participant. ⬜

## 9. Search & navigation

| Feature | Status | Phase | Notes |
|---|---|---|---|
| Quick switcher (jump to channel/DM) | ⬜ | P1b | keyboard-first |
| Channel browser (public) | 🟢 | P1b | browse + self-join dialog in both clients |
| Message search | 🟢 | P1b | substring search shipped; Postgres FTS = P2 |

## 10. Admin & operator

User management (create/deactivate/promote), server settings, message/file
retention, basic audit log. ⬜ (P7 + ongoing). Operator deploy/ops already exist
([admin-guide.md](admin-guide.md)).

## 11. Client UX (cross-cutting)

Native theming ✅(both Linux), keyboard shortcuts ⬜, message grouping by
author/time ⬜, relative timestamps ⬜, infinite-scroll history 🟡, accessibility
⬜, i18n ⬜, offline send queue ⬜.

---

## Out of scope

Deliberately **not** built (platform/growth/enterprise-suite, not core chat):

- **E2EE** (stated non-goal — self-host trust model).
- **Full threads** (Slack-style thread panes) — replaced by lightweight quote-reply.
- **Group DMs** — use a small private channel instead.
- Multi-workspace / multi-tenant; Slack Connect / federation (initially).
- Huddles/always-on audio rooms, Stories/clips, Canvas/whiteboard/docs.
- Workflow builder, app marketplace / third-party app directory.
- Message scheduling, reminders, advanced analytics dashboards, AI assistants.
- Custom-emoji sprawl, themes-as-a-feature beyond following the OS theme.

## Resolved decisions (2026-06-18)

- **Replies:** quote-reply (inline, with quoted context) — **not** full threads.
- **In as core:** emoji reactions (P1b), message search (P2, Postgres FTS),
  public self-join channels (P1b).
- **Out:** full threads, group DMs.

## Immediate next batch (proposed)

To make the app feel alive and close the gaps testing exposed, before broad
feature work:

1. **Live channel updates** + **add-member UI** (the gabriel problem) — PN slice.
2. **Unread indicators** + **mark-as-read** — minimum viable notifications.
3. **Desktop OS notifications** for DMs/mentions.
4. **Edit/delete message**, **leave/rename/archive channel**, **delete dupes**.
