"""Startup security guard for the JWT signing key."""

from __future__ import annotations

import pytest

from app.config import Settings


def test_assert_secure_rejects_default_or_short_key() -> None:
    with pytest.raises(RuntimeError):
        Settings(jwt_signing_key="too-short", allow_insecure_auth=False).assert_secure()


def test_assert_secure_allows_weak_key_when_explicitly_opted_in() -> None:
    # Should not raise.
    Settings(jwt_signing_key="too-short", allow_insecure_auth=True).assert_secure()


def test_assert_secure_accepts_strong_key() -> None:
    # Should not raise.
    Settings(jwt_signing_key="x" * 40, allow_insecure_auth=False).assert_secure()
