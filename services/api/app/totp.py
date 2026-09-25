"""TOTP (RFC 6238) and recovery codes: the pure parts, no I/O.

In-house on purpose (spec 2026-09-25-totp-server-design.md §4): TOTP is ~20 lines
of HMAC over the standard library, and owning it keeps the replay guard, the skew
window and the constant-time compare in one reviewed place. Tested against the
RFC 6238 appendix B vectors (tests/test_totp.py).

Defaults are the ones every authenticator app assumes: HMAC-SHA1, 6 digits, 30 s.
"""

from __future__ import annotations

import base64
import hmac
import secrets
import struct
from dataclasses import dataclass
from urllib.parse import quote

from .security import dummy_verify, hash_password, verify_password

PERIOD_S = 30
DIGITS = 6
# Steps accepted either side of now: ±30 s of clock drift between phone and server.
SKEW_STEPS = 1
SECRET_BYTES = 20  # RFC 4226 recommends >= 128 bits; 160 matches HMAC-SHA1's block use


def new_secret() -> str:
    """A fresh shared secret, base32 without padding (what otpauth URIs carry)."""
    return base64.b32encode(secrets.token_bytes(SECRET_BYTES)).decode().rstrip("=")


def _key(secret_b32: str) -> bytes:
    return base64.b32decode(secret_b32 + "=" * (-len(secret_b32) % 8), casefold=True)


def code_at(secret_b32: str, step: int, *, digits: int = DIGITS, digest: str = "sha1") -> str:
    """The code for time-step ``step`` (RFC 4226 HOTP with counter = step)."""
    mac = hmac.new(_key(secret_b32), struct.pack(">Q", step), digest).digest()
    offset = mac[-1] & 0x0F
    value = int.from_bytes(mac[offset : offset + 4], "big") & 0x7FFF_FFFF
    return str(value % 10**digits).zfill(digits)


def step_for(unix_time: float) -> int:
    return int(unix_time // PERIOD_S)


def match_step(secret_b32: str, code: str, now: float, last_used_step: int | None) -> int | None:
    """The accepted step for ``code`` at ``now``, or ``None``.

    Tries every step in the skew window (no early exit, so timing doesn't reveal
    which one matched) with a constant-time compare, and refuses any step not
    strictly after ``last_used_step``: RFC 6238 §5.2, a code accepted once is
    refused for the rest of its window, on every endpoint that accepts codes.
    """
    code = code.strip().replace(" ", "")
    if len(code) != DIGITS or not code.isascii() or not code.isdigit():
        return None
    center = step_for(now)
    matched: int | None = None
    for step in range(center - SKEW_STEPS, center + SKEW_STEPS + 1):
        if hmac.compare_digest(code_at(secret_b32, step), code) and matched is None:
            matched = step
    if matched is None or (last_used_step is not None and matched <= last_used_step):
        return None
    return matched


def otpauth_uri(secret_b32: str, *, issuer: str, account: str) -> str:
    """The provisioning URI. The label (``issuer:account``) and every query value are
    percent-encoded on their own, so a handle can't break the URI's structure."""
    label = quote(f"{issuer}:{account}", safe="")
    return (
        f"otpauth://totp/{label}?secret={quote(secret_b32, safe='')}"
        f"&issuer={quote(issuer, safe='')}&algorithm=SHA1&digits={DIGITS}&period={PERIOD_S}"
    )


# ---------------------------------------------------------------- recovery codes

# Crockford-style: no I, L, O, U (look-alikes of 1, 1, 0, V). 32 symbols, 5 bits each.
_ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
_LOOKUP_LEN = 4  # public id: finds the row, so a guess costs one Argon2, not ten
_SECRET_LEN = 16  # 80 bits: codes replace the second factor, so they need offline strength
RECOVERY_CODE_COUNT = 10
_LOOKALIKES = str.maketrans("OIL", "011")  # what a person reads for 0, 1, 1


@dataclass(frozen=True)
class RecoveryCode:
    """A freshly generated code: ``display`` is shown to the user once;
    ``lookup`` and ``hash`` are what the database keeps."""

    display: str
    lookup: str
    hash: str


def _rand(n: int) -> str:
    return "".join(secrets.choice(_ALPHABET) for _ in range(n))


def new_recovery_codes(count: int = RECOVERY_CODE_COUNT) -> list[RecoveryCode]:
    """``count`` codes with distinct lookups, each ``iiii-xxxx-xxxx-xxxx-xxxx``."""
    codes: list[RecoveryCode] = []
    seen: set[str] = set()
    while len(codes) < count:
        lookup = _rand(_LOOKUP_LEN)
        if lookup in seen:
            continue  # unique per user (the DB enforces it too)
        seen.add(lookup)
        secret = _rand(_SECRET_LEN)
        groups = [secret[i : i + 4] for i in range(0, _SECRET_LEN, 4)]
        codes.append(
            RecoveryCode(
                display="-".join([lookup, *groups]),
                lookup=lookup,
                # Argon2id, never a fast hash (encryption spec §7.1): a human-typed code
                # hashed with SHA-256 would fall to a GPU from a leaked dump.
                hash=hash_password(secret),
            )
        )
    return codes


def parse_recovery_code(text: str) -> tuple[str, str] | None:
    """``(lookup, secret)`` from what a user typed: case, dashes and spaces are
    forgiven, and so are the look-alikes (O→0, I/L→1). ``None`` if it can't be one."""
    cleaned = text.upper().replace("-", "").replace(" ", "")
    cleaned = cleaned.translate(_LOOKALIKES)
    if len(cleaned) != _LOOKUP_LEN + _SECRET_LEN or any(c not in _ALPHABET for c in cleaned):
        return None
    return cleaned[:_LOOKUP_LEN], cleaned[_LOOKUP_LEN:]


def verify_recovery_secret(code_hash: str | None, secret: str) -> bool:
    """Verify the secret part; with no stored hash (unknown lookup) spend one dummy
    Argon2 anyway, so a miss and a wrong code take the same time."""
    if code_hash is None:
        dummy_verify(secret)
        return False
    return verify_password(code_hash, secret)
