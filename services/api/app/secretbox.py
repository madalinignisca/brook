"""Application-layer encryption of stored secrets (spec 2026-09-22 §5).

Direct symmetric encryption with a configured keyring, not envelope encryption:
several AES-256 keys with integer ids; exactly one (explicitly chosen) is primary
and encrypts; every key in the ring decrypts, which makes rotation migration-free.

Stored format (a ``text`` column)::

    v1.<key_id>.<nonce_b64url>.<ciphertext_b64url>

AES-256-GCM with a 12-byte random nonce and the field's identity as AAD::

    brook.v1|<key_id>|<table>|<column>|<row_pk>

Callers never encrypt free-form: they name a registered :class:`Purpose` (a closed
registry; adding one is a reviewed change). **Every** decrypt failure raises
:class:`DecryptError` with an internal reason, and callers fail closed (on the
login path that is a 401, never "not enrolled" and never a 500; §5.5). Failures are
counted by reason so an operator can see a misconfigured ring (§5.8).
"""

from __future__ import annotations

import base64
import binascii
import logging
import os
import uuid
from collections import Counter
from collections.abc import Mapping
from enum import Enum
from functools import lru_cache

from cryptography.exceptions import InvalidTag
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

FORMAT = "v1"
NONCE_BYTES = 12
KEY_BYTES = 32
MAX_PLAINTEXT_BYTES = 4096
# v1 + key id + 16-byte nonce b64 + (plaintext + 16-byte tag) b64 + separators.
MAX_STORED_CHARS = 16 + 20 + 16 + ((MAX_PLAINTEXT_BYTES + 16) * 4 + 2) // 3


class Purpose(Enum):
    """The closed registry of encrypted fields: ``(table, column)``.

    Adding a member is a reviewed change (spec §3); a test enumerates them all.
    """

    TOTP_SECRET = ("totp", "secret")
    BOT_OUTBOUND_SECRET = ("bots", "outbound_secret_enc")


class DecryptError(Exception):
    """Any failure to decrypt. ``reason`` is internal (logs, metrics), never shown
    to a caller: ``malformed``, ``unknown_version``, ``unknown_key_id``,
    ``invalid_tag`` or ``missing_ring``."""

    def __init__(self, reason: str) -> None:
        super().__init__(reason)
        self.reason = reason


class KeyringError(ValueError):
    """The configured keyring is unusable (startup refuses to boot)."""


# Decrypt failures by reason, for operator detection (§5.8): any sustained non-zero
# count is an operator error; `unknown_key_id` means a key was dropped too early.
decrypt_failures: Counter[str] = Counter()


def _b64e(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode("ascii")


def _b64d(text: str) -> bytes:
    # Strict: urlsafe alphabet only, no padding in storage.
    if not text or any(c not in _B64URL for c in text):
        raise ValueError("not base64url")
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


_B64URL = frozenset("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_")


def _aad(key_id: int, purpose: Purpose, row_pk: uuid.UUID) -> bytes:
    table, column = purpose.value
    return f"brook.{FORMAT}|{key_id}|{table}|{column}|{row_pk}".encode()


def parse_keyring(spec: str) -> dict[int, bytes]:
    """Parse ``BROOK_SECRET_KEYS`` (``1:<b64url 32 bytes>,2:<...>``), strictly."""
    keys: dict[int, bytes] = {}
    if not spec:
        raise KeyringError("BROOK_SECRET_KEYS is empty")
    for entry in spec.split(","):
        key_id_text, sep, material = entry.partition(":")
        if (
            not sep
            or not (key_id_text.isascii() and key_id_text.isdigit())
            or key_id_text != key_id_text.strip()
        ):
            raise KeyringError("each key must be <id>:<base64url>")
        key_id = int(key_id_text)
        if key_id in keys:
            raise KeyringError(f"duplicate key id {key_id}")
        try:
            raw = _b64d(material)
        except (ValueError, binascii.Error) as exc:
            raise KeyringError(f"key {key_id} is not base64url") from exc
        if len(raw) != KEY_BYTES:
            raise KeyringError(f"key {key_id} is {len(raw)} bytes, need {KEY_BYTES}")
        keys[key_id] = raw
    return keys


class SecretBox:
    """Encrypt/decrypt registered secrets with the keyring."""

    def __init__(self, keys: Mapping[int, bytes], primary_id: int) -> None:
        if primary_id not in keys:
            raise KeyringError(f"primary key id {primary_id} is not in the ring")
        for key_id, raw in keys.items():
            if len(raw) != KEY_BYTES:
                raise KeyringError(f"key {key_id} is {len(raw)} bytes, need {KEY_BYTES}")
        self._keys = {key_id: AESGCM(raw) for key_id, raw in keys.items()}
        self.primary_id = primary_id

    def encrypt(self, plaintext: str, *, purpose: Purpose, row_pk: uuid.UUID) -> str:
        """Encrypt with the primary key, bound to ``purpose`` and ``row_pk``."""
        if not isinstance(purpose, Purpose):
            raise TypeError("purpose must be a registered Purpose")
        data = plaintext.encode()
        if len(data) > MAX_PLAINTEXT_BYTES:
            raise ValueError("plaintext too long")
        nonce = os.urandom(NONCE_BYTES)
        ct = self._keys[self.primary_id].encrypt(
            nonce, data, _aad(self.primary_id, purpose, row_pk)
        )
        return f"{FORMAT}.{self.primary_id}.{_b64e(nonce)}.{_b64e(ct)}"

    def decrypt(self, stored: str, *, purpose: Purpose, row_pk: uuid.UUID) -> str:
        """Decrypt a stored value. Every failure is a :class:`DecryptError`."""
        try:
            key_id, nonce, ct = self._parse(stored)
            cipher = self._keys.get(key_id)
            if cipher is None:
                raise DecryptError("unknown_key_id")
            try:
                data = cipher.decrypt(nonce, ct, _aad(key_id, purpose, row_pk))
            except InvalidTag as exc:
                raise DecryptError("invalid_tag") from exc
            return data.decode()
        except DecryptError as exc:
            decrypt_failures[exc.reason] += 1
            raise
        except UnicodeDecodeError as exc:  # authenticated but not UTF-8: still a failure
            decrypt_failures["malformed"] += 1
            raise DecryptError("malformed") from exc

    def needs_rewrap(self, stored: str) -> bool:
        """Whether ``stored`` is encrypted with a key other than the primary."""
        try:
            key_id, _, _ = self._parse(stored)
        except DecryptError:
            return False
        return key_id != self.primary_id

    @staticmethod
    def _parse(stored: str) -> tuple[int, bytes, bytes]:
        if not isinstance(stored, str) or len(stored) > MAX_STORED_CHARS:
            raise DecryptError("malformed")
        parts = stored.split(".")
        if len(parts) != 4:
            raise DecryptError("malformed")
        version, key_id_text, nonce_text, ct_text = parts
        if version != FORMAT:
            raise DecryptError("unknown_version")
        if not (key_id_text.isascii() and key_id_text.isdigit()):
            raise DecryptError("malformed")
        try:
            nonce, ct = _b64d(nonce_text), _b64d(ct_text)
        except (ValueError, binascii.Error) as exc:
            raise DecryptError("malformed") from exc
        if len(nonce) != NONCE_BYTES or len(ct) < 16:
            raise DecryptError("malformed")
        return int(key_id_text), nonce, ct


# The well-known key the dev escape hatch substitutes (BROOK_ALLOW_INSECURE_AUTH=1
# without a ring): like the dev JWT key, public by design, never for production.
_DEV_KEY_ID = 0
_DEV_KEY = b"brook-insecure-dev-secret-key!!!"  # nosec B105 - dev-only, 32 bytes
log = logging.getLogger(__name__)


@lru_cache
def get_secret_box() -> SecretBox:
    """The process-wide SecretBox from settings (validated at startup)."""
    from .config import get_settings

    s = get_settings()
    if s.secret_keys is None:
        if not s.allow_insecure_auth:  # assert_secure() refuses this at startup
            raise KeyringError("no secret keyring configured")
        log.warning("secret keyring: using the insecure dev key (key id %d)", _DEV_KEY_ID)
        return SecretBox({_DEV_KEY_ID: _DEV_KEY}, _DEV_KEY_ID)
    if s.secret_primary_key_id is None:  # never guess the encrypting key
        raise KeyringError("BROOK_SECRET_PRIMARY_KEY_ID is not set")
    box = SecretBox(parse_keyring(s.secret_keys.get_secret_value()), s.secret_primary_key_id)
    log.info("secret keyring: primary key id %d", box.primary_id)
    return box
