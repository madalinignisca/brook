# Attachments: server design

Status: draft for Heavy review. Issue #64 (MVP+). Owner decisions of 2026-09-25 are marked
**(owner)**.

## 1. Goals / non-goals

**Goals:**
- Send files in a channel.
- Every filename is safe on every OS and device.
- Deletion follows the message and the account (GDPR).
- Simple to run and back up on one small server.

**Non-goals:**
- Server-side previews or thumbnails: clients make them from the bytes, since server-side
  image processing is a classic attack surface.
- Virus scanning.
- End-to-end encryption.
- Deduplication.
- Horizontal scaling.

## 2. Storage: plain files (owner)

- **The api stores file bytes on the local filesystem**, in `BROOK_FILES_DIR`. Production
  uses `/var/lib/brook/files`; development uses a compose volume. There is **no object
  storage (no S3, no RustFS) in dev or prod**: one code path everywhere.
  - Owner: "for a humble human with a little server on budget, local filesystem is a
    bless". Object storage earns its keep for horizontal scaling, which this isn't.
  - It supersedes ARCHITECTURE.md's "bytes bypass the api" for this deployment class, and
    the implementation PR updates ARCHITECTURE.md and SECURITY.md §4 to match.
- **Layout:** `<dir>/<aa>/<file_id>`, where `aa` is the first two hex characters of the
  UUID, so no directory holds more than about 1/256 of the files. The name on disk is the
  UUID only **(owner)**: no filename, no path tricks, no collisions, no enumeration.
- **Durability:** the volume is Oracle-replicated. Point-in-time recovery comes from OCI's
  scheduled volume backups (owner's console setting), which cover this directory with no
  extra tooling.
- **Shared host:** busuioc is also the git server. Uploads are refused with
  `507 file.no_space` when the filesystem's free space would drop below
  `BROOK_FILES_MIN_FREE` (default **5 GB**), so attachments can never fill the disk that git
  needs.
- **systemd:** `brook-api.service` gets `StateDirectory=brook` (for `/var/lib/brook`, owned
  by `brook`, writable under `ProtectSystem=strict`). SELinux: the default `var_lib_t`
  labelling applies, nothing custom, and never `chcon`.

## 3. Wire (under `/api/v1`)

**Create:** `POST /channels/{id}/files {filename, size, content_type, client_id?}`, for
members of a non-archived channel:
```
201 {file: FileOut(status: "pending"), upload_url: "/api/v1/files/{id}/content"}
```
- `size` ≤ 100 MB, and ≤ the user's remaining quota: `413 file.too_large` /
  `413 file.quota_exceeded`.
- `client_id` makes it idempotent like messages (sync spec §4): a retry returns the same
  file.

**Upload:** `PUT /files/{id}/content`, by the uploader. The body is the raw bytes (not
multipart).
- The server **streams the body to its own part file**, `<id>.<random>.part`, opened with
  `O_EXCL`, counting bytes. It stops and answers `413` the moment the count exceeds the
  declared `size`, and deletes its part. It never buffers the whole file in memory.
  - Why a part file per PUT: a client retrying after a timeout while its first PUT is still
    streaming would otherwise interleave two bodies into one file and commit a corrupt one.
- When the stream ends: `422 file.size_mismatch` if the count differs from `size`.
  Otherwise it fsyncs, then **under the file row's lock and only while `status ==
  pending`**, atomically renames its part to `<id>` and marks the file `committed`.
- The answer is `200 FileOut`.
- A PUT that finishes second (or arrives after the commit) finds the file committed,
  deletes its own part, and gets `409 {code: "file.already_committed", details: FileOut}`.
  - Why the body: an outbox retry that lost to its own earlier attempt must confirm its
    bytes won, by comparing `details.sha256` with its own. It is the one error body the
    client is meant to read. It carries no user input, so it is safe to read, unlike a 422.
- Caddy caps the request body at 100 MB on this path, a second limit in front of the api.
- A second PUT to a committed file is `409 conflict`. Uploads don't resume: a failed upload
  is restarted with PUT (the part is overwritten).

**Attach:** `POST /channels/{id}/messages {body, attachments: [file_id, ...]}` (≤ 10
files).
- Each file must be `committed`, uploaded by the author, uploaded to **this** channel, and
  not yet attached. Otherwise `422 file.not_attachable`.
- Attaching happens in the message insert's transaction, and is covered by `client_id`.
- `MessageOut`, `message.new` and `message.update` carry `attachments: [FileOut]`. `PATCH`
  can't change attachments in MVP+.

**Download:** `GET /files/{id}/content` (the §5 headers are set on this route by the api,
and Caddy adds nosniff and the CSP on `/api/v1/files/*/content` as a second layer), by a member of the file's channel, with the normal
bearer token. A `pending` file is `404`, since it doesn't exist yet for anyone. Auth is checked
when the request starts, so a long download outlives token expiry, and a resume is just a
new `Range` request with a fresh token. The api streams the file (Starlette `FileResponse`), and **`Range` is
supported** (206 and `Content-Range`), so an interrupted download resumes. There are no
signed URLs, so none can leak through logs or sharing.

**Delete:** `DELETE /files/{id}`, by the uploader or a channel owner, removes the row and
the bytes. An attached file leaves its message showing "file removed", and the delete
re-stamps the message's `seq`, so offline caches drop it.

**`FileOut`:**
```
{id, filename, original_name, size, content_type, status, created_at, uploader_id, channel_id, sha256}
```
`sha256` is computed while streaming the upload, so a cache can check what it holds.

## 4. Filenames (owner)

`filename` is **sanitised and transliterated to ASCII at create time**, and is the name
every client saves under:
1. **Transliterate** with `anyascii` (ISC; not the GPL Unidecode), e.g.
   `Ștefan–raport.pdf` becomes `Stefan-raport.pdf`.
2. Keep only the **last path segment** (split on `/` and `\`).
3. Remove control characters and every bidi control character (RLO/LRO/isolates).
4. Replace `<>:"/\|?*` with `_`, and collapse whitespace.
5. Trim leading and trailing dots and spaces.
6. Prefix Windows device names with `_`, even with an extension (`CON`, `PRN`, `AUX`,
   `NUL`, `COM1-9`, `LPT1-9`).
7. Cap at 255 bytes, **keeping the extension**.
8. If nothing is left, use `file`, plus the extension when there is one.

`original_name` keeps the name as typed, minus the characters removed in step 3. It is
display text ("alt text") only, **never** a filesystem name. The FAQ explains the
transliteration.

## 5. Downloads can't execute on our origin

Every download response carries:
- `Content-Disposition: attachment; filename="<ascii>"; filename*=UTF-8''<same, percent-encoded>`;
- `Content-Type`: the stored type, or `application/octet-stream` for anything in the
  active-content list (`text/html`, `image/svg+xml`, `application/xhtml+xml`, `text/xml`,
  `application/javascript`, …);
- `X-Content-Type-Options: nosniff`;
- `Content-Security-Policy: sandbox; default-src 'none'`;
- `Cross-Origin-Resource-Policy: same-origin`.

**Clients** save to Downloads with the platform save dialog, append ` (1)` on a name
clash, never auto-open, and treat `content_type` as untrusted except for choosing a
preview.

## 6. Limits (owner defaults)

- 100 MB per file.
- 5 GB per user, counting committed and pending files.
- The `BROOK_FILES_MIN_FREE` disk floor. It counts the declared sizes of `pending`
  uploads as already used, so parallel uploads can't jointly overshoot it.
- Creates are rate-limited per user, through a new bucket in the #36 limiter.

## 7. Lifecycle

- **Sweep** (a periodic task in the api):
  - removes `pending` files older than 1 h, with their `<id>.*.part` files. A part file is
    only ever removed when it too is older than the 1 h window (by mtime), so the sweep
    never deletes one mid-upload;
  - removes committed files never attached after 24 h;
  - removes files on disk that have no row (after a crash between rename and commit, or an
    account delete's cascade).
- **A message delete** removes its attachments' rows and bytes in the same request.
- **An account delete** cascades the rows; the sweep removes the orphaned bytes.

## 8. Data model (one migration)

`files`:

| Column | Type | Notes |
|---|---|---|
| `id` | uuid PK | also the file's name on disk |
| `channel_id` | FK | cascade |
| `uploader_id` | FK users | cascade |
| `filename` | text | sanitised ASCII |
| `original_name` | text | |
| `size` | bigint | |
| `content_type` | text | |
| `sha256` | text | null until committed |
| `status` | text | `pending` or `committed` |
| `client_id` | uuid | null; unique with `uploader_id` |
| `message_id` | FK messages | null, set null |
| `created_at`, `committed_at` | timestamptz | |

With an index on `(status, created_at)` and on `uploader_id`.

## 9. Tests (each seen red under mutation)

1. **Sanitiser table:** traversal, RLO, control characters, Windows device names (with and
   without an extension), reserved characters, dots and spaces, 300-byte names keeping the
   extension, the empty-name fallback, and transliteration of Latin diacritics, Cyrillic and
   CJK.
2. **Streaming cap:** a body over the declared size gives 413 and leaves no part file behind;
   a short body gives 422; memory stays bounded (a large upload through a test client).
2a. **Overlapping PUTs** to one pending file: exactly one commits, the stored bytes and
    sha256 are exactly that body's, the other gets `409 file.already_committed` whose
    `details` is the committed FileOut, and no part files remain.
3. **Atomic commit:** a crash before the rename leaves no committed row and no final file;
   the sha256 is stored.
4. **Attach rules:** another user's file, another channel's file, a pending file and an
   already attached file are each refused.
5. **Authorisation:** a non-member can't create, upload or download; after leaving a
   channel the member can't download.
6. **Download headers:** always `attachment`, nosniff, CSP sandbox; active-content types
   forced to `octet-stream`; `Range` gives 206.
7. **Limits:** the cap, the quota and the free-space floor (with a mocked `statvfs`); the
   rate limit on creates.
8. **Sweep:** old pending files and their parts go; committed-unattached files go after
   24 h; orphan bytes go; attached files are never removed.
9. **A message delete** removes the bytes and re-stamps the message's `seq`.
