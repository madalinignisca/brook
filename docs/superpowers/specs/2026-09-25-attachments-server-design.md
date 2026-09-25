# Attachments: server design

Status: draft for Heavy review. Issue #64 (MVP+). The wire extends PROTOCOL.md §1
(`/channels/{id}/files`, `/files/{id}/commit`, `/files/{id}`) and §5 (upload states).
Owner decisions of 2026-09-25 are marked **(owner)**.

## 1. Goals / non-goals

**Goals:**
- Send files in a channel.
- Bytes go directly between the client and object storage; the api only authorises and
  signs.
- Every filename is safe on every OS and device.
- Deletion follows the message and the account (GDPR).

**Non-goals:**
- Server-side previews or thumbnails: clients make them from the bytes, since server-side
  image processing is a classic attack surface.
- Virus scanning.
- End-to-end encryption.
- Deduplication.

## 2. Storage

- **RustFS**: in development (compose), and on the production host natively under systemd,
  storing on the local disk **(owner)**. MinIO's community edition was archived in 04/2026.
  RustFS 1.0.0 was verified on 2026-09-25 to enforce the POST policy (`content-length-range`,
  `key`, `Content-Type`) and to verify presigned GET signatures.
- The api uses only the portable S3 surface: POST policy, presigned GET, HEAD, DELETE.
- One bucket, `brook-files`, and one key per object: `files/<uuid>` **(owner)**. No filename
  in the key: no path tricks, no collisions, no enumeration.
- **Served under the same host,** path-style: `https://<host>/brook-files/...`. Caddy
  proxies that path to RustFS on loopback, so there is no new DNS name or certificate.
  Same origin means a stored file must never render in a browser (§5).

## 3. Wire (under `/api/v1`)

**`POST /channels/{id}/files {filename, size, content_type, client_id?}`**, for members of
a non-archived channel. `client_id` (UUID) makes it idempotent like messages (sync spec
§4): a retry after a lost response returns the same `file_id`, with a fresh policy if the
file is still pending, instead of a second pending row counted against the quota.
```
201 {file_id, upload: {url, fields}, expires_in: 600}
```
- The **POST policy** pins `key=files/<file_id>`, `content-length-range: [1, size]`
  (≤ 100 MB) and the declared `Content-Type`, with a 10-minute expiry.
- The row is created `pending`, with the sanitised name and the original (§4).
- `413 file.too_large` above the per-file cap; `413 file.quota_exceeded` above the user's
  quota; `403` if not a member.

The client uploads the bytes directly to `upload.url` with `upload.fields`.

**`POST /files/{id}/commit`**, by the uploader:
- The server `HEAD`s the object. It must exist with a size of at least 1 and at most the
  declared size, and the declared content type.
- The **HEAD-verified size** is stored in `files.size`, and that value is what counts
  toward the quota. The policy allows anything up to the declared size, so the declared
  value isn't trusted.
- On success the file becomes `committed` and `200 FileOut` is returned. A second commit
  is idempotent (200 with the same body).
- `409 file.not_uploaded` if the object isn't there; `422 file.mismatch` on a size or type
  mismatch, in which case the object is deleted.

**Attach:** `POST /channels/{id}/messages {body, attachments: [file_id, ...]}` (≤ 10
files).
- Each file must be `committed`, uploaded by the author, uploaded to **this** channel, and
  not yet attached. Otherwise `422 file.not_attachable`.
- Attaching is part of the message insert (same transaction) and is covered by the
  message's `client_id` idempotency (sync spec §4).

**`GET /files/{id}`**, by a member of the file's channel:
```
200 {download_url, expires_in: 600, file: FileOut}
```
The presigned GET carries the response overrides of §5.

**`DELETE /files/{id}`**, by the uploader or a channel owner: removes the row and the
object. A file attached to a message leaves its message showing "file removed", and the
delete **re-stamps that message's `seq`** (sync spec §2), or every offline cache would
keep the file.

**Uploads don't resume:** a failed upload is started again (a new policy, the same
`client_id`). **Downloads do:** presigned GET supports `Range`, verified on RustFS 1.0.0
(`206`, correct `Content-Range`, with the forced disposition and type still applied). After
the URL expires mid-download, the client calls `GET /files/{id}` again and resumes with
`Range` on the new URL.

**`FileOut`**:
```
{id, filename, original_name, size, content_type, created_at, uploader_id, channel_id}
```
`MessageOut` gains `attachments: [FileOut]`, and so do the WebSocket `message.new` and
`message.update` payloads. `PATCH` can't change a message's attachments in MVP+. `FileOut`
also carries `etag` (the object ETag at commit), so a cache can check what it holds.

## 4. Filenames (owner)

- `filename` is **sanitised and transliterated to ASCII at upload**, and is the name every
  client saves under:
  1. **Transliterate** with `anyascii` (ISC; not the GPL Unidecode), e.g.
     `Ștefan–raport.pdf` becomes `Stefan-raport.pdf`.
  2. Keep only the **last path segment** (split on `/` and `\`).
  3. Remove control characters and every bidi control character (RLO/LRO/isolates, so
     `invoice‮fdp.exe` can't disguise itself).
  4. Replace `<>:"/\|?*` with `_`, and collapse whitespace.
  5. Trim leading and trailing dots and spaces.
  6. Refuse Windows device names, even with an extension (`CON`, `PRN`, `AUX`, `NUL`,
     `COM1-9`, `LPT1-9`) by prefixing `_`.
  7. Cap at 255 bytes, **keeping the extension**.
  8. If nothing is left, use `file`, plus the extension when there is one.
- `original_name` keeps the name as typed, with only the characters removed in step 3.
  Clients show it as display text ("alt text") and **never** use it as a filesystem name.
- The FAQ states that server and apps force this transliteration for a consistent
  experience across every OS and device.

## 5. Downloads can't execute on our origin

Every presigned GET sets these response overrides (signed, so a client can't strip them):
- `response-content-disposition: attachment; filename="<ascii>"; filename*=UTF-8''<same, percent-encoded>`
  (RFC 6266/5987);
- `response-content-type`: the committed type, or `application/octet-stream` for anything
  in the active-content list (`text/html`, `image/svg+xml`, `application/xhtml+xml`,
  `text/xml`, `application/javascript`, …).

Caddy adds headers on `/brook-files/*` that no signature can remove:
- `X-Content-Type-Options: nosniff`;
- `Content-Security-Policy: sandbox; default-src 'none'`;
- `Cross-Origin-Resource-Policy: same-origin`.

**Signed URLs stay out of logs.** Caddy has no access log on this site today. If one is
ever added, it must drop the query string on `/brook-files/*`, because a signed URL is a
bearer capability for its lifetime. The api never logs `download_url` or the policy.

**Clients** save to Downloads (with the platform save dialog), append ` (1)` on a name
clash, never auto-open, and treat `content_type` as untrusted for anything but choosing a
preview.

## 6. Limits and quotas (owner defaults)

- **100 MB per file.** It's enforced in the policy, where storage refuses the upload
  mid-transfer, and again at commit.
- **5 GB per user, counting committed and pending files.** Pending files expire, so they
  can't block the quota for long.
- **Upload starts are rate-limited** per user (the #36 limiter, a new bucket) to stop
  pending-row floods.

## 7. Lifecycle and deletion

- **Orphan sweep:** a periodic task in the api deletes `pending` files older than 1 h
  (object and row) and committed files never attached after 24 h.
- **A message delete** (soft, a tombstone) deletes its attachments' objects and rows in the
  same request. Sync tombstones drop attachments (sync spec §2).
- **An account delete** cascades to rows. The object deletion is done by the same sweep,
  which finds objects without rows via a bucket listing, so nothing depends on one request
  finishing.
- **Backups:** the objects are not in the database dumps. The file backup target is a
  separate owner decision (proposed: an Oracle Object Storage bucket in Frankfurt), and
  the attachments feature ships before it with that stated.

## 8. Data model (one migration)

`files`:

| Column | Type | Notes |
|---|---|---|
| `id` | uuid PK | also the object key |
| `channel_id` | FK | cascade |
| `uploader_id` | FK users | cascade |
| `filename` | text | sanitised ASCII |
| `original_name` | text | |
| `size` | bigint | declared, then verified |
| `content_type` | text | |
| `status` | text | `pending` or `committed` |
| `message_id` | FK messages | null, set null |
| `created_at`, `committed_at` | timestamptz | |

With an index on `(status, created_at)` for the sweep, and on `uploader_id` for the quota.

## 9. Tests (each seen red under mutation)

1. **Sanitiser table:** traversal, RLO, control characters, Windows device names (with and
   without an extension), reserved characters, dots and spaces, 300-byte names keeping the
   extension, the empty-name fallback, and transliteration of Latin diacritics, Cyrillic and
   CJK.
2. **The policy pins key, size and type**, asserted on the signed policy document. A live
   RustFS test in the compose-backed E2E: an oversized upload is refused, and a different
   key is refused.
3. **Commit:** a missing object gives 409; a size or type mismatch gives 422 and deletes
   the object; committing twice returns 200.
4. **Attach rules:** another user's file, another channel's file, a pending file and an
   already attached file are each refused.
5. **Authorisation:** a non-member can't upload, commit or download; after leaving a
   channel the member can't download.
6. **The GET override** always forces `attachment`, and active-content types are forced to
   `octet-stream`.
7. **Quota and cap:** 413s; the rate limit on upload starts.
8. **Sweep:** old pending files are removed; committed-unattached files are removed after
   24 h; attached files are never removed.
9. **Message delete** removes the attachments.
