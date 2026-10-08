# SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
# SPDX-License-Identifier: AGPL-3.0-or-later

"""Health endpoint tests."""

from __future__ import annotations

import httpx
import pytest

from app import __version__, config
from app.config import DEFAULT_SOURCE_URL


async def test_health_ok(client: httpx.AsyncClient) -> None:
    resp = await client.get("/health")
    assert resp.status_code == 200
    body = resp.json()
    assert body["status"] == "ok"
    assert "version" in body


async def test_health_default_source_url(client: httpx.AsyncClient) -> None:
    # Exact dict: an extra or renamed key fails.
    resp = await client.get("/health")
    assert resp.json() == {"status": "ok", "version": __version__, "source_url": DEFAULT_SOURCE_URL}


async def test_health_reports_configured_source_url(
    client: httpx.AsyncClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("BROOK_SOURCE_URL", "https://git.example.org/fork")
    # The client fixture's create_app() already cached settings, and httpx's ASGITransport
    # does not run the lifespan: drop the cache here so the route sees the new env.
    config.get_settings.cache_clear()
    resp = await client.get("/health")
    assert resp.json()["source_url"] == "https://git.example.org/fork"


async def test_health_is_public_and_ignores_a_bad_token(client: httpx.AsyncClient) -> None:
    # AGPL section 13: users must be able to read it before signing in; a sent token is
    # ignored, never refused.
    resp = await client.get("/health", headers={"Authorization": "Bearer not-a-token"})
    assert resp.status_code == 200
    assert "source_url" in resp.json()
