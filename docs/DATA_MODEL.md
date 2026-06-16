# Data model

> PostgreSQL. Draft v0 — entities and relationships, not final DDL.

## Entities

```
User ──< Membership >── Channel ──< Message
 │                         │            │
 │                         │            └─< Attachment ──► File (MinIO object)
 │                         └─< Call (room) ──< CallParticipant
 └─< Bot (owned)                          
Channel ──< ChannelBot >── Bot
Message ── (author) ─► User | Bot
```

## Tables (sketch)

**users** — `id (uuidv7) · handle · display_name · avatar_file_id? · global_role (admin|member) · status (active|deactivated) · created_at`
> **Global roles & bootstrap:** Brook is **single-workspace per server** (one self-hosted instance = one organization; no multi-tenant). Two global roles: **admin** (manage users/bots/server settings) and **member**. The **first user created bootstraps as `admin`**; admins promote/deactivate others. `status=deactivated` handles **deprovisioning** (federated user removed upstream, or offboarded) without deleting history. Channel-level `owner|member` (in `memberships`) is separate from global role.
**local_credentials** — `user_id · password_hash (argon2id)` (only for local accounts)
**totp** — `user_id · secret (enc) · activated_at` (+ `recovery_codes`: `user_id · code_hash · used_at?`)
**identities** — `id · user_id · provider (oidc|ldap) · issuer/server · subject_or_dn · created_at`
> Links a user to a federated identity. A user is *local* (has `local_credentials`) and/or *federated* (has `identities` rows). See [AUTH.md](AUTH.md).
**refresh_tokens** — `id · user_id · expires_at · revoked`

**channels** — `id · kind (dm|channel) · name? · topic? · created_by · created_at`
> A 1:1 DM is just a `kind=dm` channel with exactly two members.

**memberships** — `channel_id · user_id · role (owner|member) · joined_at` (PK: channel_id+user_id)

**messages** — `id (uuidv7) · channel_id · author_type (user|bot) · author_id · body · created_at · edited_at? · deleted_at?`
> `id` is UUIDv7 → time-sortable, drives pagination (`before=<id>`).

**attachments** — `message_id · file_id`
**files** — `id · owner_id · bucket · object_key · filename · size · content_type · status (pending|committed) · created_at`
> Bytes live in MinIO; this row is metadata + the object pointer. Access via presigned URLs. **`status`**: created `pending` on presigned PUT; flips to `committed` after `POST /files/{id}/commit` verifies the object exists and size/type ≤ caps. Only `committed` files attach to messages; `pending`/orphaned objects are swept (see [PROTOCOL.md](PROTOCOL.md) §5).

**bots** — `id · owner_id · name (slug, used in /name) · webhook_url · inbound_secret_hash · outbound_secret_enc · created_at`
> Two secrets, two storage strategies (see [SECURITY.md](SECURITY.md) §5): inbound is only *verified* → store a **hash**; outbound must be *produced* → store **encrypted** (retrievable).

**devices** — `id · user_id · platform (apns|fcm) · push_token · created_at` (for mobile push wake / call invites)
**channel_bots** — `channel_id · bot_id · added_by` (which bots participate where)

**calls** — `id (room_id) · channel_id · started_by · started_at · last_active_at · ended_at?`
**call_participants** — `call_id · user_id · joined_at · left_at?`
> `last_active_at` drives **orphan cleanup**: a call with no active participants for the call-idle TTL ([SECURITY.md](SECURITY.md) §7) is closed and its SFU room torn down — guards against client crashes mid-call.

## Notes

- **Soft-delete** messages (`deleted_at`) to keep history/thread integrity.
- **Presence** is ephemeral (in-memory / cache, not a table).
- **Bot as author:** `messages.author_type` distinguishes user vs bot so a bot is a first-class participant (feature 6).
- Indexes: `messages(channel_id, id desc)` for history; `memberships(user_id)` for channel list.
- **Encryption at rest is the operator's infrastructure concern** (encrypted volume / Postgres / SSE), not modeled here — the schema is agnostic to it. See [SECURITY.md](SECURITY.md) §4a.
