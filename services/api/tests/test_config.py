# SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
# SPDX-License-Identifier: AGPL-3.0-or-later

"""Startup security guard for the JWT signing key."""

from __future__ import annotations

from pathlib import Path

import pytest

from app.config import DEFAULT_SOURCE_URL, SOURCE_URL_MAX_BYTES, Settings


def test_assert_secure_rejects_default_or_short_key() -> None:
    with pytest.raises(RuntimeError):
        Settings(jwt_signing_key="too-short", allow_insecure_auth=False).assert_secure()


def test_assert_secure_allows_weak_key_when_explicitly_opted_in() -> None:
    # Should not raise.
    Settings(jwt_signing_key="too-short", allow_insecure_auth=True).assert_secure()


def test_assert_secure_accepts_strong_key() -> None:
    # Should not raise.
    Settings(jwt_signing_key="x" * 40, allow_insecure_auth=False).assert_secure()


# --- BROOK_SOURCE_URL (AGPL section 13, #300) ---------------------------------------------
#
# Every case builds Settings with a strong JWT key and goes through assert_secure(), so an
# accepted case cannot fail on the JWT guard and a refused case cannot "pass" on the JWT
# guard's different message. The keyring is valid in every test (conftest's autouse fixture).


def _settings(source_url: str) -> Settings:
    return Settings(jwt_signing_key="x" * 40, allow_insecure_auth=False, source_url=source_url)


def test_source_url_empty_env_means_default(monkeypatch: pytest.MonkeyPatch) -> None:
    # Compose passes an unset `${BROOK_SOURCE_URL:-}` as "": that must count as unset.
    monkeypatch.setenv("BROOK_SOURCE_URL", "")
    s = Settings(jwt_signing_key="x" * 40)
    assert s.source_url == DEFAULT_SOURCE_URL
    s.assert_secure()


def test_source_url_unset_means_default(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("BROOK_SOURCE_URL", raising=False)
    s = Settings(jwt_signing_key="x" * 40)
    assert s.source_url == DEFAULT_SOURCE_URL
    s.assert_secure()


REFUSED_SOURCE_URLS = [
    "ftp://example.com/x",
    "javascript:alert(1)",
    "file:///etc/passwd",
    "github.com/madalinignisca/brook",
    "https://",
    "https:///path",
    "https://user:secret-pw@example.com/",
    "https://user@example.com/",
    "https://@example.com/",
    "https://exa mple.com/",
    " https://example.com/",
    "https://example.com:99999/",
    "https://[::1/",
    "https://[v1.x]/",  # IPvFuture: urlsplit allows it, core refuses it
    "https://[fe80::1%25eth0]/",  # zone id
    "https://a<b.com/",
    "https://a%b.com/",
    "https://good.example\\evil/",
    "https://1.2.3.256/",
    "https://example.123/",
    "https://example.123./",
    "https://xn--a.com/",
    "https://e.com/" + "a" * 2035,  # 2049 bytes
]


@pytest.mark.parametrize("value", REFUSED_SOURCE_URLS)
def test_source_url_malformed_is_refused(value: str) -> None:
    with pytest.raises(RuntimeError, match="BROOK_SOURCE_URL"):
        _settings(value).assert_secure()


ACCEPTED_SOURCE_URLS = [
    "http://example.com",
    "https://bücher.example/brook",
    "https://xn--bcher-kva.example/brook",
    "https://xn--fa-hia.de/",  # valid IDNA 2008 (faß); the IDNA 2003 codec refuses it
    "https://192.0.2.1/brook",
    "https://192.0.2.1./brook",
    "https://[2001:db8::1]/brook",
    "https://e.com/" + "a" * 2034,  # exactly 2048 bytes
]


@pytest.mark.parametrize("value", ACCEPTED_SOURCE_URLS)
def test_source_url_valid_is_accepted(value: str) -> None:
    assert len(value.encode("utf-8")) <= SOURCE_URL_MAX_BYTES
    _settings(value).assert_secure()


def test_source_url_boundary_is_exact() -> None:
    assert len(("https://e.com/" + "a" * 2034).encode()) == SOURCE_URL_MAX_BYTES


def test_source_url_error_never_echoes_the_value() -> None:
    # A userinfo value may hold a password; the message must not repeat it.
    with pytest.raises(RuntimeError) as exc:
        _settings("https://user:secret-pw@example.com/").assert_secure()
    assert "secret-pw" not in str(exc.value)


def test_source_url_non_utf8_value_gets_the_guard_message() -> None:
    # An env value that is not valid UTF-8 arrives with surrogate escapes; it must be
    # refused with the guard's message, not crash with UnicodeEncodeError, and not be echoed.
    with pytest.raises(RuntimeError, match="BROOK_SOURCE_URL") as exc:
        _settings("https://example.com/\udcff").assert_secure()
    assert "udcff" not in str(exc.value).lower()


def test_source_url_guard_ignores_allow_insecure_auth() -> None:
    # The dev hatch is about the JWT key and keyring only, not a malformed link.
    with pytest.raises(RuntimeError, match="BROOK_SOURCE_URL"):
        Settings(
            jwt_signing_key="x" * 40, allow_insecure_auth=True, source_url="javascript:alert(1)"
        ).assert_secure()


def test_malformed_source_url_stops_the_server_at_startup(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The wiring: the lifespan runs assert_secure(), which must reach the source-URL guard.
    from fastapi.testclient import TestClient

    from app import config
    from app.main import create_app

    monkeypatch.setenv("BROOK_DATABASE_URL", f"sqlite+aiosqlite:///{tmp_path / 'wiring.db'}")
    monkeypatch.setenv("BROOK_JWT_SIGNING_KEY", "test-signing-key-at-least-32-bytes-long!")
    monkeypatch.setenv("BROOK_SOURCE_URL", "javascript:alert(1)")
    config.get_settings.cache_clear()
    with pytest.raises(RuntimeError, match="BROOK_SOURCE_URL"), TestClient(create_app()):
        pass
    config.get_settings.cache_clear()
