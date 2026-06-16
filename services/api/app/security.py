"""Password hashing (Argon2id), JWT access tokens, and refresh-token helpers."""

from __future__ import annotations

import contextlib
import hashlib
import secrets
import uuid
from datetime import timedelta
from typing import Any

import jwt
from argon2 import PasswordHasher
from argon2.exceptions import Argon2Error

from .config import Settings
from .models import utcnow

_hasher = PasswordHasher()  # Argon2id defaults

# A fixed hash used to normalize timing when a login targets a missing user,
# so response time doesn't reveal whether a handle exists (user enumeration).
_DUMMY_HASH = _hasher.hash("brook-timing-normalization-dummy")  # noqa: S106


def hash_password(password: str) -> str:
    """Hash a password with Argon2id."""
    return _hasher.hash(password)


def verify_password(password_hash: str, password: str) -> bool:
    """Verify a password against an Argon2id hash (False on any mismatch/invalid)."""
    try:
        return _hasher.verify(password_hash, password)
    except Argon2Error:
        return False


def dummy_verify(password: str) -> None:
    """Spend ~one Argon2 verify of time without revealing anything (timing parity)."""
    with contextlib.suppress(Argon2Error):
        _hasher.verify(_DUMMY_HASH, password)


def needs_rehash(password_hash: str) -> bool:
    """Whether a stored hash should be upgraded to current Argon2 parameters."""
    return _hasher.check_needs_rehash(password_hash)


def create_access_token(settings: Settings, user_id: uuid.UUID, role: str) -> str:
    """Mint a short-lived access JWT."""
    now = utcnow()
    payload: dict[str, Any] = {
        "sub": str(user_id),
        "role": role,
        "type": "access",
        "iat": int(now.timestamp()),
        "exp": int((now + timedelta(seconds=settings.access_ttl_seconds)).timestamp()),
    }
    return jwt.encode(payload, settings.jwt_signing_key, algorithm=settings.jwt_algorithm)


def decode_access_token(settings: Settings, token: str) -> dict[str, Any]:
    """Decode and validate an access JWT.

    Enforces presence of ``exp``/``iat``/``sub`` and that ``type == 'access'``.
    Raises ``jwt.PyJWTError`` on any failure.
    """
    data: dict[str, Any] = jwt.decode(
        token,
        settings.jwt_signing_key,
        algorithms=[settings.jwt_algorithm],
        options={"require": ["exp", "iat", "sub"]},
    )
    if data.get("type") != "access":
        raise jwt.InvalidTokenError("not an access token")
    return data


def new_refresh_token() -> tuple[str, str]:
    """Return ``(raw_token, token_hash)``; only the hash is persisted."""
    raw = secrets.token_urlsafe(48)
    return raw, hash_token(raw)


def hash_token(raw: str) -> str:
    """Hash a refresh token for storage/lookup (SHA-256 hex)."""
    return hashlib.sha256(raw.encode()).hexdigest()
