# Application-level secret encryption

> Status: **revisions required** — stage-3 review findings in §10 · 2026-09-22
> Touches: [SECURITY.md](../../SECURITY.md) §4a/§5, [DATA_MODEL.md](../../DATA_MODEL.md), [ROADMAP.md](../../ROADMAP.md) Phase 0b

## 1. Problem

Two specced fields must be stored encrypted and **read back in plaintext** by `api`:

| Field | Needed in | Why hashing is not an option |
|---|---|---|
| `totp.secret` | **Phase 0b (next)** | The server recomputes the expected TOTP code on every login. |
| `bots.outbound_secret_enc` | Phase 3 | The server produces the HMAC over outbound webhook calls. |

`SECURITY.md` §5 currently says the outbound secret is *"stored encrypted (envelope
encryption via the app's key / operator secret store)"* — that clause names three different
schemes and picks none. `DATA_MODEL.md` marks `totp.secret (enc)` with no scheme at all.

And there is no key: `services/api/app/config.py` holds exactly one piece of key material,
`jwt_signing_key`. `deploy/.env.example` matches. **Phase 0b cannot be implemented as
specced.**

### Why disk encryption does not answer this

`SECURITY.md` §4a puts encryption at rest on the operator (encrypted volumes, encrypted
PostgreSQL, SSE buckets) and requires the app to stay agnostic to it. That is correct for
bulk data and must not change.

It does not cover these two fields, because **a `pg_dump` taken from an encrypted volume is
plaintext.** Disk encryption defends against stolen hardware; it does nothing against a
leaked backup, a misconfigured replica, or an SQL-injection read. For message bodies that is
acceptable — the operator can read them anyway, per the trust model in §8. For a TOTP secret
it is not: a leaked TOTP secret grants *future authentication*, silently defeating the second
factor for every enrolled user, with no signal that it happened.

### The seam this resolves

§4a ("the app must never depend on at-rest encryption") and §5 ("stored encrypted via the
app's key") read as a contradiction. They are not — they describe two different things that
share a name:

- **Bulk data** (messages, files, metadata) → the operator's metal. The app stays agnostic.
- **Re-creatable credentials** (TOTP secrets, bot signing secrets) → the app's key, because
  only the app knows which columns are credentials.

Neither encrypts the other's territory.

## 2. Goals / non-goals

**Goals**

- One small, reviewed primitive for encrypting app-managed secrets at rest in the database.
- Ciphertexts bound to the row they belong to, so they cannot be relocated between rows.
- Key rotation without downtime and without a schema migration.
- Operable by a small-business admin: one environment variable, no external dependency.
- Startup refuses a missing or weak key, exactly as it already does for the JWT key.

**Non-goals**

- **Not E2EE.** Unchanged non-goal (`SECURITY.md` scope note, §2, §8; `AUTH.md`).
- **Not** encrypting message bodies, attachments, or files. See the invariant in §3.
- **Not** a KMS/Vault integration. A provider seam is left open (§5.1); nothing more.
- **Not** replacing anything currently hashed. `refresh_tokens` (SHA-256),
  `local_credentials.password_hash` (Argon2id), `totp.recovery_codes`, and
  `bots.inbound_secret_hash` are verify-only and stay hashed. Hashing is the stronger
  choice wherever the server never needs the plaintext back.

## 3. The invariant that keeps this safe

> **This key protects only secrets that can be re-created. Nothing irreplaceable is ever
> encrypted with it.**

This is the load-bearing rule and it belongs in `SECURITY.md` §5, not just here.

Lose the key today and the blast radius is bounded and recoverable: enrolled users re-enrol
TOTP (an admin "reset 2FA" path is required anyway, for lost phones), bot owners re-issue
outbound secrets. No history is lost.

The moment message bodies are encrypted with the same mechanism, key loss becomes permanent,
unrecoverable data loss — and a self-hosted operator *will* eventually lose the key. Writing
the rule down now is what prevents a well-meaning change in a later phase from turning a
recoverable incident into a destroyed archive.

## 4. Scope

In scope, and only these:

- `totp.secret` — AAD `("totp", "secret", <user_id>)`
- `bots.outbound_secret_enc` — AAD `("bots", "outbound_secret_enc", <bot_id>)`

Any future field must be justified against the invariant in §3 before it is added.

## 5. Design

### 5.1 Key material

A **keyring**: several keys, each with a small integer id. The highest id is the primary and
is the only key used to encrypt; every key in the ring can decrypt.

```
BROOK_SECRET_KEYS=1:<base64url 32 bytes>,2:<base64url 32 bytes>
```

- One variable. The primary is **the highest id present** — rotation is "add a key with the
  next number", and there is no second variable that can point at a key that isn't there.
- `BROOK_SECRET_KEYS_FILE` is accepted as an alternative source, for Docker secrets and
  `systemd` credentials. This is the whole of the "provider seam": a future Vault/KMS
  provider populates the same ring, and nothing above this layer changes.
- Startup logs the active primary key id (never key material), so a mis-numbered key is
  visible rather than silent.
- `make init` generates a ring with a single key `1:<random>`, matching how it already
  generates `BROOK_JWT_SIGNING_KEY`.

**Rejected:** a separate `BROOK_SECRET_PRIMARY_KEY_ID`. It is one more thing to get wrong,
and "highest wins" needs no validation beyond "ids are unique".

### 5.2 Ciphertext format

Stored in a `text` column:

```
v1.<key_id>.<nonce_b64url>.<ciphertext_b64url>
```

- `v1` — format version, so a future change is detectable rather than ambiguous.
- `<key_id>` — which key encrypted this. This is what makes rotation possible without a
  migration: old rows say which old key they need.
- `<nonce>` — 12 random bytes per encryption. AES-GCM with random nonces is safe to roughly
  2^32 encryptions under one key; a self-hosted Brook will not approach it, and rotation
  resets the count regardless.
- base64url without padding, so no `=`, `+` or `/` to escape anywhere.

### 5.3 Algorithm and row binding

**AES-256-GCM**, via `cryptography`'s `AESGCM`, with **AAD** set to the field's identity:

```
AAD = "brook.v1|<table>|<column>|<row_pk>"
```

The AAD is authenticated but not stored — it is reconstructed at decrypt time from the row
already in hand.

This is the reason for choosing AES-GCM over Fernet, which has no AAD. Without row binding,
an attacker with database **write** access can copy another user's encrypted TOTP secret into
their own row; it decrypts cleanly, because nothing ties the ciphertext to a user, and they
authenticate with that user's second factor. With AAD, the same move fails with `InvalidTag`.

Row primary keys are UUIDv7 and immutable, so a stable AAD is guaranteed.

### 5.4 API

`services/api/app/crypto.py`, deliberately small:

```python
class SecretBox:
    def __init__(self, keys: Mapping[int, bytes]) -> None: ...
    def encrypt(self, plaintext: str, *, aad: tuple[str, str, str]) -> str: ...
    def decrypt(self, stored: str, *, aad: tuple[str, str, str]) -> str: ...
    def needs_rewrap(self, stored: str) -> bool: ...
```

`needs_rewrap` mirrors the existing `needs_rehash` in `security.py`: it returns true when a
value was encrypted under a non-primary key.

### 5.5 Rotation

Zero-downtime, four steps, no migration:

1. Append a key with the next id. Both keys decrypt; the new one becomes primary.
2. Restart `api`. New writes use the new key; old rows still read.
3. **Lazy rewrap:** wherever a secret is decrypted on a normal code path (TOTP verify,
   outbound webhook signing), `needs_rewrap` triggers a re-encrypt and save. Most rows
   migrate through ordinary use, the same way Argon2 parameters already upgrade via
   `needs_rehash`.
4. An admin command rewraps the remainder; once none are left, drop the old key.

### 5.6 Startup guard

Extend the existing `Settings.assert_secure()` rather than introducing a second concept. It
refuses to boot when the ring is missing, any key is not exactly 32 bytes after decoding, or
ids are duplicated — unless `BROOK_ALLOW_INSECURE_AUTH=1`, which is already the documented
local-dev escape hatch for the JWT key.

## 6. Sequencing — why now

`totp` does not exist yet. Landing this **before** Phase 0b means the column is written
encrypted from its first row, and there is never a migration that reads plaintext secrets out
of a live database and writes them back encrypted. Landing it after means exactly that
migration, on the most sensitive column in the schema.

`bots` (Phase 3) gets the same treatment for free.

## 7. Testing

Per `docs/QUALITY.md` and the 70/100 target:

- Round-trip: encrypt → decrypt returns the plaintext.
- **Row binding:** a ciphertext produced for row A fails with `InvalidTag` when decrypted
  with row B's AAD. This is the test the whole design exists for.
- Tampering: flipping a byte in the ciphertext fails; it does not return garbage.
- Rotation: a value encrypted under key 1 still decrypts after key 2 becomes primary;
  `needs_rewrap` reports true for it and false after rewrap.
- Unknown key id and unknown format version each produce a clear, distinguishable error
  rather than a generic failure.
- Startup: missing ring, wrong-length key, and duplicate ids each refuse to boot; the
  documented escape hatch allows dev.
- **Prove the tests can fail:** remove the AAD argument from the decrypt path and confirm the
  row-binding test goes red before restoring it. A row-binding test that has never been
  observed failing is decoration.

## 8. Risks and accepted costs

- **New dependency:** `cryptography`. Not currently a direct dep — PyJWT on HS256 does not
  pull it in. Accepted under the stack rule that genuinely hard, well-solved problems (crypto,
  TLS) use an established package rather than a local implementation. It also becomes
  available for future asymmetric JWT signing.
- **`AESGCM` lives under `cryptography.hazmat`.** Mitigated by confining it to one small
  module with no configurable knobs, and sending that module through `auth-reviewer` (Fable)
  and `code-reviewer` before merge — the authn/authz escalation trigger applies here.
- **Key loss** costs TOTP re-enrolment and bot secret re-issue. Bounded by §3 and documented
  in the admin guide, including the rotation procedure.
- **Operator burden:** one more secret to back up. Called out in `docs/admin-guide.md`
  alongside `BROOK_JWT_SIGNING_KEY`, which has the same property.

## 9. Documentation changes this requires

- `SECURITY.md` §5 — replace the "envelope encryption via the app's key / operator secret
  store" clause with this scheme, and add the §3 invariant.
- `SECURITY.md` §4a — one paragraph distinguishing operator-managed bulk data from
  app-managed credentials, so the two sections stop reading as a contradiction.
- `SECURITY.md` §7 — key length and rotation expectations alongside the other limits.
- `DATA_MODEL.md` — document the stored format for `(enc)` columns.
- `deploy/.env.example` and `make init` — the new variable.
- `docs/admin-guide.md` — back up the key; rotate the key; what happens if it is lost.

---

## 10. Stage-3 review findings (2026-09-22) — REVISIONS REQUIRED

Reviewed by Codex (gpt-5.6-terra). Vibe timed out; `auth-reviewer` pending. **The design is
not approved for implementation until §§1–9 are revised per the below.** Verdict on the
primitive itself: "the cryptographic primitive is fine" — the weaknesses are in framing, key
management, and rotation.

### Must fix — errors in the current text

1. **§5.3's attack description is wrong.** The doc claims an attacker copies a *victim's*
   ciphertext into their *own* row and thereby passes the victim's 2FA. That does not work:
   the attacker never holds the victim's plaintext secret, so cannot compute codes from it.
   The real attack is the inverse — an attacker copies a secret **they already know** (from
   their own enrolment) into the **victim's** row, then generates valid codes for the
   victim's account. AAD still stops this, so the conclusion stands, but the justification
   must be rewritten.

2. **§5.1 "highest key id is primary" is a footgun, not a simplification.** A key added with
   a lower id is silently ignored while the operator believes rotation happened. A reused id
   with different material makes every ciphertext under that id permanently undecryptable.
   Replace with an explicit `BROOK_SECRET_PRIMARY_KEY_ID`, validated at startup (primary
   exists in ring; ids unique, strictly parsed, never reused even after retirement).

3. **§5.5 lazy rewrap has a real race.** Request A decrypts the old ciphertext → the user
   re-enrols TOTP → request A writes back its stale plaintext, destroying the new enrolment.
   Rewrap must be compare-and-swap (`UPDATE … WHERE user_id = :id AND secret = :old`) or use
   an optimistic version column; a failed CAS means "someone else updated it, do nothing".
   Authentication must complete on successful decrypt regardless of whether rewrap succeeds,
   and a rewrap failure must never turn a successful login into a 500.

4. **§5.5 "zero downtime" is incomplete for multi-instance deployments.** Every `api`
   instance must hold the superset ring *before* any instance starts encrypting under the new
   primary, or an old instance cannot read rows a new one wrote.

5. **§5.2's "2^32" is stated too casually.** That is the NIST operational ceiling for
   random-IV GCM under one key — global per key across all processes and restarts — not a
   comfortable estimate or a target. Say the volume here is nowhere near it, and state an
   operational bound well below it.

6. **§5.6 must not allow a plaintext fallback.** `BROOK_ALLOW_INSECURE_AUTH=1` must never
   cause a missing or malformed ring to degrade into plaintext storage. Dev uses an explicit
   test key; production always requires a valid ring.

7. **§3's invariant is currently just a comment.** A free-form `aad: tuple[str, str, str]`
   lets any caller encrypt anything. Replace with a **closed registry of encrypted-field
   purposes** — each entry naming field, AAD construction, owner and recovery procedure — and
   a test enumerating them. Still governance, not a cryptographic guarantee, but enforceable
   in review.

### Must add — omissions

8. **Backup/key-retention lifecycle.** An old `pg_dump` needs the ring that existed when it
   was taken. Retiring a key after a verified rewrap makes older backups partially
   unrestorable unless retired keys are escrowed for the full backup-retention period. This
   needs an explicit policy in the admin guide.

9. **Redaction.** TOTP secrets, provisioning URIs/QR payloads and outbound bot secrets must
   never reach logs, traces, exception output, ORM reprs, or admin exports.

10. **Replay.** AAD binds record identity but not secret *generation*. A DB writer can replay
    an older ciphertext into the same row. Out of scope for AAD; needs audit/integrity
    controls if it matters.

11. **Availability under tampering.** A DB writer can corrupt ciphertext or substitute an
    unknown key id, locking users out. AAD detects this but does not restore access — define
    alerts and an operator recovery path.

12. **Key-source semantics.** Precedence and mutual exclusion between `BROOK_SECRET_KEYS` and
    `BROOK_SECRET_KEYS_FILE`; file permissions/ownership; strict parsing of whitespace and
    trailing newlines; error messages that do not leak material.

13. **Field/storage constraints.** Maximum plaintext size, ciphertext column length, strict
    format parsing, and decryption failures mapped to non-enumerating auth responses.

14. **Recovery must be designed, not asserted.** "Re-creatable" is not satisfied by naming an
    admin reset path. Key loss needs a break-glass procedure that does not itself depend on
    the affected TOTP population, plus a definition of who may invoke it. Bot-secret reissue
    requires coordinating the receiving bot, so key loss causes integration outages.

### Framing corrections

15. Call this **"application-layer protection against DB-only disclosure"**, not "encryption
    at rest". It protects against a leaked dump, a leaked read replica, or SQL injection that
    reads but cannot execute. It does **not** protect against API RCE, host/root compromise,
    container environment inspection, or a whole-host backup that includes deployment
    secrets. §8 of `SECURITY.md` should gain "DB-only disclosure" as an explicit attacker
    capability and state plainly that host compromise defeats this entirely.

16. This is **not** envelope encryption — it is direct symmetric encryption with a configured
    data-encryption keyring. `SECURITY.md` §5's existing wording should be corrected too.
