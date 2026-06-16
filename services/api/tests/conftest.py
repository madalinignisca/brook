"""Test fixtures: an isolated DB and an async HTTP client per test.

Defaults to a per-test SQLite file. Set ``BROOK_TEST_DATABASE_URL`` (e.g. a
Postgres URL in CI) to run the same suite against another backend; schema is
created and dropped per test for isolation.
"""

from __future__ import annotations

import os
from collections.abc import AsyncIterator
from pathlib import Path

import httpx
import pytest

from app import config, db
from app.main import create_app
from app.models import Base


@pytest.fixture
async def client(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> AsyncIterator[httpx.AsyncClient]:
    """Yield an HTTP client bound to an app backed by a fresh per-test database."""
    external = os.environ.get("BROOK_TEST_DATABASE_URL")
    monkeypatch.setenv(
        "BROOK_DATABASE_URL", external or f"sqlite+aiosqlite:///{tmp_path / 'test.db'}"
    )
    monkeypatch.setenv("BROOK_JWT_SIGNING_KEY", "test-signing-key-at-least-32-bytes-long!")

    # Reset cached settings + engine so this test gets its own DB.
    config.get_settings.cache_clear()
    db._engine = None
    db._sessionmaker = None
    await db.init_models()

    app = create_app()
    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://test") as ac:
        yield ac

    # Tear down schema (matters for a shared external DB) and dispose the engine.
    engine = db._engine
    if engine is not None:
        async with engine.begin() as conn:
            await conn.run_sync(Base.metadata.drop_all)
        await engine.dispose()
    db._engine = None
    db._sessionmaker = None
    config.get_settings.cache_clear()
