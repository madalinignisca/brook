# Application-level secret encryption

> Status: **revised after stage-3 review** — ready for implementation · 2026-09-22
> Reviewed by: Codex (gpt-5.6-terra), `auth-reviewer` (Fable). Vibe timed out — see §12.
> Touches: [SECURITY.md](../../SECURITY.md) §4a/§5/§8, [DATA_MODEL.md](../../DATA_MODEL.md),
> [AUTH.md](../../AUTH.md) §1, [ROADMAP.md](../../ROADMAP.md) Phase 0b

## 1. Problem

Two specced fields must be stored encrypted and **read back in plaintext** by `api`:

| Field | Needed in | Why hashing is not an option |
|---|---|---|
| `totp.secret` | **Phase 0b (next)** | The server recomputes the expected TOTP code on every login. |
| `bots.outbound_secret_enc` | Phase 3 | The server produces the HMAC over outbound webhook calls. |

`SECURITY.md` §5 currently says the outbound secret is *"stored encrypted (envelope
encryption via the app's key / operator secret store)"* — that clause names three different
schemes, picks none, and **"envelope encryption" is the wrong term** for what is needed here
(see §5.0). `DATA_MODEL.md` marks `totp.secret (enc)` with no scheme at all.

And there is no key: `services/api/app/config.py` holds exactly one piece of key material,
`jwt_signing_key`. `deploy/.env.example` matches. **Phase 0b cannot be implemented as
specced.**

## 2. Threat model — state it precisely

This is **application-layer protection against DB-only disclosure**. It is *not* "encryption
at rest", and the distinction decides whether the design is honest.

**Protects against:**

- A leaked `pg_dump`. `docs/admin-guide.md` documents `pg_dump > brook-backup-$(date).sql`
  as *the* backup procedure — a plaintext file produced on the encrypted volume and then
  stored wherever backups live.
- A leaked or misconfigured read replica.
- SQL injection that can **read** rows but cannot execute code in `api`.

**Does not protect against, at all:**

- RCE in `api`, or host/root compromise.
- Inspection of the container's environment or `/proc/<pid>/environ`.
- A whole-host backup that captures `deploy/.env` alongside the dump (see §11).

Host compromise defeats this completely, and `SECURITY.md` §8 must say so. What this buys is
narrow and real: of everything in a database dump, the TOTP secret is the **only credential
usable as-is** — passwords are Argon2id, refresh tokens are SHA-256 of 384-bit randoms. It is
also precisely the factor meant to survive a phished password.

This raises TOTP secrets to the same tier as `BROOK_JWT_SIGNING_KEY`, which already grants
total auth bypass if leaked. It does not put them above it.

### Why disk encryption does not answer this

A `pg_dump` taken from an encrypted volume is plaintext — the volume is mounted, that is the
point. Disk encryption defends against stolen hardware. `SECURITY.md` §4a is correct for bulk
data and does not change.

### The seam this resolves

§4a ("the app must never depend on at-rest encryption") and §5 ("stored encrypted via the
app's key") read as a contradiction. They describe two different things sharing a name:

- **Bulk data** (messages, files, metadata) → the operator's metal. The app stays agnostic.
- **Re-creatable credentials** (TOTP secrets, bot signing secrets) → the app's key, because
  only the app knows which columns are credentials.

## 3. The invariant that keeps this safe

> **This key protects only secrets that can be re-created. Nothing irreplaceable is ever
> encrypted with it.**

Lose the key and the damage is bounded: users re-enrol TOTP, bot owners re-issue secrets. No
history is lost. Encrypt message bodies with the same mechanism and key loss becomes
permanent data loss — and a self-hosted operator *will* eventually lose the key.

This is enforced structurally, not by comment: §5.4's closed registry is the only way to
reach the primitive, and adding an entry is a reviewed change.

## 4. Goals / non-goals

**Goals** — one reviewed primitive; ciphertexts bound to their field identity; rotation with
no schema migration; one environment variable; fail-closed startup **and** fail-closed
decrypt.

**Non-goals**

- **Not E2EE.** Unchanged (`SECURITY.md` scope note, §2, §8; `AUTH.md`).
- **Not** encrypting message bodies, attachments, or files (§3).
- **Not** a KMS/Vault integration. A provider seam only (§5.1).
- **Not** replacing `local_credentials.password_hash` (Argon2id) or `refresh_tokens.token_hash`.
  The latter is SHA-256 over `secrets.token_urlsafe(48)` = 384 bits, looked up by indexed
  equality — brute-force infeasible, no timing signal, correct as-is.

> **Removed in revision:** the original draft listed `bots.inbound_secret_hash` here as
> "verify-only, stays hashed". That is wrong and must not be propagated into `SECURITY.md`
> §5 — see §11.

## 5. Design

### 5.0 What this is not

This is **direct symmetric encryption with a configured data-encryption keyring**. It is
**not envelope encryption**, which requires a separate wrapping key or KMS layer. `SECURITY.md`
§5's wording is incorrect and §11 corrects it.

### 5.1 Key material

A **keyring**: several keys, each with an integer id. Exactly one is primary and is the only
key used to encrypt; every key in the ring can decrypt.

```
BROOK_SECRET_KEYS=1:<base64url 32 bytes>,2:<base64url 32 bytes>
BROOK_SECRET_PRIMARY_KEY_ID=2
```

- **The primary is explicit.** Startup refuses to boot if it is absent from the ring.
- **Key ids are never reused**, including after retirement — a reused id makes every
  ciphertext carrying it permanently undecryptable.
- `BROOK_SECRET_KEYS_FILE` is the preferred source in `deploy/docker-compose.yml`: values in
  `environment:` are visible via `docker inspect` and `/proc/<pid>/environ`. Precedence,
  required file mode, and strict parsing (trailing newline, whitespace) are specified below.
- Held as pydantic `SecretStr`, so `repr`, validation errors and settings dumps never print
  it. `BROOK_JWT_SIGNING_KEY` should become `SecretStr` at the same time.
- Startup logs the **primary key id only**, never material.
- `make init` generates `1:<random>` with `BROOK_SECRET_PRIMARY_KEY_ID=1`, as it already does
  for `BROOK_JWT_SIGNING_KEY`.

> **Revised:** the first draft derived the primary as "highest id present". A key added with
> a lower id would then be silently ignored while the operator believed rotation had
> happened. Both reviewers flagged it.

### 5.2 Ciphertext format

Stored in a `text` column:

```
v1.<key_id>.<nonce_b64url>.<ciphertext_b64url>
```

`v1` is the format version; `<key_id>` is what makes migration-free rotation possible;
`<nonce>` is exactly 12 random bytes, enforced on parse. base64url, unpadded.

**Nonce bound.** 2^32 is the NIST operational ceiling for random-IV GCM **per key, globally,
across every process and restart** — a limit, not a target. TOTP enrolments and bot-secret
writes are orders of magnitude below it; the operational bound is set at 2^24 per key, at
which point rotation is required. Rotation only resets the count if the new key is genuinely
new and every writer has it.

### 5.3 Algorithm and field binding

**AES-256-GCM** via `cryptography`'s `AESGCM`, with **AAD** set to the field's identity:

```
AAD = "brook.v1|<key_id>|<table>|<column>|<row_pk>"
```

Reconstructed at decrypt time from the row in hand; never stored. The key id is authenticated
too, closing any future downgrade argument if `v2` uses a different algorithm.

**Honest justification.** The first draft claimed AAD stops an attacker with DB write access
from stealing a second factor, and described the attack backwards. Both corrections matter:

- The described attack does not work. Copying a *victim's* ciphertext into the *attacker's*
  row leaves the attacker needing the victim's authenticator. The meaningful direction is the
  reverse — plant a ciphertext whose plaintext the **attacker knows** into the **victim's**
  row, then log in as the victim with a phished password and the attacker's own authenticator.
- More importantly, **an attacker with DB write access does not need any of this.** They can
  insert a `refresh_tokens` row with a `token_hash` they chose and call `POST /auth/refresh`
  to mint a full session — no password, no 2FA. Or `DELETE FROM totp`, or set
  `global_role='admin'`. AAD stops none of that and cannot.

So AAD is **not** load-bearing against a DB-write adversary, and under §2's actual threat
model (read-only disclosure) it is never exercised at all. It is kept because it is free and
it makes two *other* failures fail closed:

- An **application bug** that decrypts the wrong row's ciphertext.
- **Cross-column planting** — a `bots.outbound_secret_enc` value landing in `totp.secret`.

That is a smaller claim than the original, and it is the true one. It does not change the
choice of AES-GCM over Fernet, which costs nothing.

Row primary keys are immutable, which is the property AAD needs. (`models.py` uses
`uuid.uuid4`, not the UUIDv7 that `DATA_MODEL.md` claims — immutability holds either way, but
this document will not assert v7.)

### 5.4 API — a closed registry, not a free-form helper

A free-form `encrypt(plaintext, aad=(...))` lets any caller encrypt anything, which makes §3
a comment. Callers instead name a registered **purpose**:

```python
class Purpose(Enum):
    TOTP_SECRET         = ("totp", "secret")
    BOT_OUTBOUND_SECRET = ("bots", "outbound_secret_enc")

class SecretBox:
    def __init__(self, keys: Mapping[int, bytes], primary_id: int) -> None: ...
    def encrypt(self, plaintext: str, *, purpose: Purpose, row_pk: uuid.UUID) -> str: ...
    def decrypt(self, stored: str, *, purpose: Purpose, row_pk: uuid.UUID) -> str: ...
    def needs_rewrap(self, stored: str) -> bool: ...
```

Adding a `Purpose` is a reviewed change that must be justified against §3, and a test
enumerates every member so additions are visible in the diff. `needs_rewrap` mirrors the
existing `needs_rehash` in `security.py`.

Max plaintext length and max ciphertext column length are bounded; malformed input is
rejected on parse rather than passed to the cipher.

### 5.5 Fail-closed on decrypt failure — the contract that matters most

**Any decrypt failure on `totp.secret` is an authentication failure.** Never a fall-through to
"not enrolled", never a 500.

This is the single most important line in the document. Three realistic operator errors —
an old key dropped before rewrap completed, a corrupted row, a missing ring under the dev
escape hatch on a box with real enrolments — would otherwise turn the encryption layer into a
**2FA-bypass switch that flips on operator error, silently**, since users simply stop being
asked for a code.

Required behaviour:

- `InvalidTag`, unknown key id, unknown format version, missing ring → **401**, generic error
  code, no distinguishing detail to the caller.
- Logged at ERROR with user id and key id. Never the plaintext, never the ciphertext.
- Caught **at the call site**. `errors.py`'s `_unhandled_exception` would otherwise render a
  500 with a traceback — a distinguishable oracle on the login path, and a support ticket.
- The rewrap CLI reports row counts per key id (`WHERE secret LIKE 'v1.1.%'`) so an operator
  sees "17 rows still on key 1" *before* dropping it, rather than discovering it as 17
  locked-out users.

### 5.6 Rotation

Migration-free, but not as simple as the first draft implied:

1. Append a key with a new, never-before-used id. **Deploy the superset ring to every `api`
   instance first**, before any instance is told to encrypt with it — otherwise an old
   instance cannot read what a new one wrote.
2. Move `BROOK_SECRET_PRIMARY_KEY_ID` to the new key and restart.
3. **Lazy rewrap**, with compare-and-swap:

   ```sql
   UPDATE totp SET secret = :new WHERE user_id = :id AND secret = :old
   ```

   A failed CAS means someone else updated the row — do nothing. Without this, a request that
   decrypted before a concurrent re-enrolment will write its stale plaintext back and destroy
   the new enrolment.

   Rewrap runs **only after a successful TOTP verification**, not merely after a successful
   decrypt — otherwise every failed login by an unauthenticated party causes a DB write.
   Rewrap is best-effort maintenance: if it fails, the login still succeeds, and the failure
   is logged and retried later. A rewrap failure must never poison the request transaction.

4. An admin **CLI** command (not HTTP) rewraps the remainder in batches with CAS and progress
   by key id, then verifies zero rows remain on the old key before it is dropped.

**Key retention vs. backups.** An old `pg_dump` can only be restored with the ring that
existed when it was taken. Dropping a key after a verified rewrap is safe for live data and
**silently breaks older backups**. Retired keys are escrowed for the full backup-retention
period. This belongs in the admin guide, not folklore.

### 5.7 Startup guard

Extends the existing `Settings.assert_secure()`. Refuses to boot when the ring is missing, a
key is not exactly 32 bytes decoded, ids are duplicated, or the primary id is not in the ring.

`BROOK_ALLOW_INSECURE_AUTH=1` substitutes a **fixed, well-known dev key** — matching the
existing `_DEV_JWT_KEY` posture. It **must never** mean plaintext storage, and it must never
mean TOTP is silently disabled.

## 6. Scope

`totp.secret` and `bots.outbound_secret_enc`. Nothing else without a §3 justification.

## 7. Requirements this design places on the Phase 0b TOTP spec

The reviews surfaced auth issues that are **not** this document's to solve but which this
document's threat model depends on. Phase 0b must not ship without them:

1. **Recovery codes must use Argon2id**, via the existing `PasswordHasher`. Currently
   `AUTH.md` §1 says "hashed" and `DATA_MODEL.md` says `code_hash`. Human-typeable codes carry
   ~40–52 bits; SHA-256 of one is GPU-brute-forceable from a leaked dump in hours. An attacker
   then logs in with password + recovery code and **never touches the encrypted TOTP secret** —
   which would make this entire design worthless against its own headline threat. They are
   verified rarely, so Argon2's cost is irrelevant.
2. **The `{totp_required}` intermediate state needs a real token.** `PROTOCOL.md` defines the
   two-call flow but nothing says what carries "password already verified". Required: a
   dedicated `type="totp_pending"` token, ~5 min, bound to `sub`, refused by
   `get_current_user` (the existing `type == "access"` check is the hook), single-use.
3. **Replay guard.** RFC 6238 §5.2 — a code accepted once must be refused for the rest of its
   window. `DATA_MODEL.md`'s `totp` table needs a `last_used_step` column; add it now, while
   the table shape is being decided.
4. **Rate limiting on `/auth/totp` is a precondition of the endpoint, not deferred
   hardening.** With ±1 step tolerance there are ~3 valid codes per million; unthrottled, it
   falls in ~10^5 requests. Must also cover recovery-code submission.
5. **Enrolment must not silently overwrite an activated secret** — require the current code or
   password re-auth, and a full `access` session (not `totp_pending`). The otpauth URI is
   returned once at enrolment and never by a later `GET`. Pending enrolments expire.
6. **Admin "reset 2FA"** — §3 leans on it as the key-loss recovery path. It needs: admin
   re-authentication immediately before use; one user per call over HTTP with bulk only via
   host CLI; revocation of the target's refresh tokens; an append-only auth event record
   (enrol, activate, reset-with-actor, recovery-code-used, key rotation); and admins cannot
   reset their own.
7. **Secret generation:** `secrets.token_bytes(20)`, base32 for the otpauth URI, SHA-1 HMAC
   per RFC 6238 default for authenticator compatibility. Constant-time code comparison.

## 8. Sequencing

`totp` does not exist yet. Landing this **before** Phase 0b means the column is encrypted from
its first row and no migration ever reads plaintext secrets out of a live database. `bots`
(Phase 3) gets it free.

## 9. Testing

- Round-trip; tampered ciphertext fails; 12-byte nonce enforced on parse.
- Field binding: a ciphertext made for one purpose/row fails under another. Both reviewers
  noted the test is symmetric and therefore still correct despite the prose error in §5.3.
- Rotation: key-1 value decrypts after primary moves to key 2; `needs_rewrap` true, then false.
- CAS rewrap: simulate a concurrent re-enrolment between decrypt and rewrap; assert the new
  enrolment survives.
- Unknown key id / unknown version / malformed input each give distinguishable internal errors
  and an indistinguishable 401 externally.
- **Fail-closed integration test (the one that matters):** enrol under ring `{1:K1}`, restart
  with ring `{2:K2}` only, then password + valid code → **401**, not "enrolled=false", not 500.
- Startup refusal: missing ring, wrong key length, duplicate ids, primary not in ring.
- **Prove the tests can fail.** Mutating the crypto module is not sufficient — the
  authorization branch in `login` is what must be observed going red. Delete the
  TOTP-required branch and confirm a test fails before restoring it.

## 10. Risks and accepted costs

- **New dependency:** `cryptography` — not currently direct (PyJWT on HS256 does not pull it
  in). Accepted under the stack rule for crypto/TLS. Also unlocks asymmetric JWT later.
- **`AESGCM` is under `cryptography.hazmat`.** Confined to one module with no configurable
  knobs; the authn/authz escalation trigger applies, so `auth-reviewer` re-reviews the
  implementation, not just this design.
- **Key loss** costs TOTP re-enrolment and bot-secret re-issue, the latter requiring the
  receiving bot operator to coordinate — key loss causes integration outages, not just
  inconvenience. Break-glass recovery must not itself depend on the affected TOTP population.
- **Operator burden:** one more secret to back up — **separately** (§11).

## 11. Documentation changes this requires

- `SECURITY.md` §5 — replace the "envelope encryption" clause with this scheme (and the
  correct term), add the §3 invariant, and **remove the claim that a hash of the inbound bot
  secret can verify an HMAC** (see below).
- `SECURITY.md` §4a — distinguish operator-managed bulk data from app-managed credentials.
- `SECURITY.md` §8 — add "DB-only disclosure" as an explicit attacker capability, and state
  that API/host compromise defeats application secret encryption entirely.
- `SECURITY.md` §7 — key length, rotation expectations, nonce bound.
- `DATA_MODEL.md` — stored format for `(enc)` columns; name Argon2id on `recovery_codes`; add
  `last_used_step` to `totp`.
- `AUTH.md` §1 — recovery codes are Argon2id, not merely "hashed".
- `deploy/.env.example`, `deploy/Makefile` (`make init`), `deploy/docker-compose.yml` (use the
  file source, not `environment:`).
- `docs/admin-guide.md` — **the key ring is backed up separately from database dumps**
  (password manager or separate vault), never in the same archive or bucket. Without this
  sentence the obvious operator move — `tar deploy/ brook-backup-*.sql` — defeats the design
  in its own headline scenario. Plus: rotation procedure, retired-key escrow, what key loss
  costs.

**Carried forward — an error in `SECURITY.md` §5 this design must not inherit.** §5 says
the inbound bot secret is stored as a hash because `api` "only needs to verify" an HMAC over
body + timestamp. **Verifying an HMAC requires the key itself; whatever can verify can forge.**
A hash of the key verifies nothing. Phase 3 must either switch inbound to a bearer secret in a
header (then hashing a server-generated >=256-bit token is correct) or move the inbound secret
into the encrypted set. Not this design's scope, but flagged here so the §11 edit does not
propagate it.

## 12. Review log

| Reviewer | Outcome |
|---|---|
| **Codex** (gpt-5.6-terra) | Full review. "The cryptographic primitive is fine." Found: AAD attack described backwards; highest-id-primary is a footgun; lazy rewrap lost-update race; 2^32 stated too casually; invariant unenforceable as written; missing backup/key-retention, redaction, replay, key-source semantics. |
| **`auth-reviewer`** (Fable) | Four blocking findings: fail-closed semantics unspecified; recovery codes reopen the leaked-dump threat; `totp_pending`/replay/rate-limit unspecified; admin reset lacks authz and audit. Independently found the same AAD error. Verified as sound: the threat model, SHA-256 for refresh tokens, the §3 invariant, the primitive, the startup guard, the sequencing. |
| **Vibe** | **Did not complete** — killed at a 300 s timeout (exit 143) on a direct CLI invocation, not through the `vibe-reviewer` wrapper. Prompt was ~450 words plus four file reads. Stage 3b is therefore **not satisfied**; re-run when the CLI is fixed. |

All findings above are incorporated. The remaining known gap is the missing Vibe pass.
