# storage — object storage (MinIO)

S3-compatible blob storage for file transfers (feature 5).

## Model
- File **bytes** live here; file **metadata** lives in Postgres (see [../../docs/DATA_MODEL.md](../../docs/DATA_MODEL.md)).
- Clients upload/download via **presigned URLs** minted by `api` — bytes never proxy through `api`.
  - Upload: `api` → presigned **PUT** (single object, short TTL) → client PUTs over HTTPS.
  - Download: `api` → presigned **GET** → client GETs over HTTPS.
- "Upload to server, pull anytime when online" maps exactly to: uploader PUTs; recipient GETs when next online.

## Security
- Served over **HTTPS**. Presigned URLs are short-lived, single-object capabilities (see [../../docs/SECURITY.md](../../docs/SECURITY.md)).
- **Encryption at rest (SSE) is the operator's choice and must not affect the app** — the app is agnostic to it (see [../../docs/SECURITY.md](../../docs/SECURITY.md) §4a). Bucket policy: least privilege.

## This directory will hold
MinIO container config, bucket bootstrap (lifecycle/retention), and example policies.
