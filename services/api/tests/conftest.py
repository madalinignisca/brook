"""Test fixtures: an isolated DB and an async HTTP client per test.

Defaults to a per-test SQLite file. Set ``BROOK_TEST_DATABASE_URL`` (e.g. a
Postgres URL in CI) to run the same suite against another backend; schema is
created and dropped per test for isolation.
"""

from __future__ import annotations

import os
from collections.abc import AsyncIterator, Iterator
from pathlib import Path

import httpx
import pytest
from fastapi.testclient import TestClient

from app import config, db, ratelimit, secretbox
from app.main import create_app
from app.models import Base

# A throwaway test keyring (never used outside tests): key 1, 32 zero-ish bytes.
TEST_SECRET_KEYS = "1:" + "dGVzdC1zZWNyZXQta2V5LTMyLWJ5dGVzLWxvbmchISE"


@pytest.fixture(autouse=True)
def _test_secret_keyring(monkeypatch: pytest.MonkeyPatch) -> Iterator[None]:
    """Every test boots with a valid (throwaway) secret keyring (§5.7 startup guard),
    and builds its own SecretBox from it (never one cached by an earlier test)."""
    monkeypatch.setenv("BROOK_SECRET_KEYS", TEST_SECRET_KEYS)
    monkeypatch.setenv("BROOK_SECRET_PRIMARY_KEY_ID", "1")
    secretbox.get_secret_box.cache_clear()
    yield
    secretbox.get_secret_box.cache_clear()


@pytest.fixture(autouse=True)
def _files_dir(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[None]:
    """Each test stores attachments in its own directory, with no disk-space floor
    (a CI runner's free space must not decide test outcomes; the floor is tested
    explicitly), and a fresh upload-start bucket."""
    from app.routers import files as files_router

    monkeypatch.setenv("BROOK_FILES_DIR", str(tmp_path / "files"))
    monkeypatch.setenv("BROOK_FILES_MIN_FREE_BYTES", "0")
    files_router._create_buckets.clear()
    yield
    files_router._create_buckets.clear()


@pytest.fixture(autouse=True)
def _fresh_limiter() -> Iterator[None]:
    """Each test starts with an empty auth limiter (all test clients share one IP)."""
    ratelimit.get_limiter.cache_clear()
    yield
    ratelimit.get_limiter.cache_clear()


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


@pytest.fixture
def sync_client(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Iterator[TestClient]:
    """A Starlette TestClient (needed for WebSocket tests) on a fresh database.

    The app's lifespan runs inside the TestClient's own event loop, so the engine
    is created there; sharing an engine across loops breaks asyncpg.
    """
    external = os.environ.get("BROOK_TEST_DATABASE_URL")
    monkeypatch.setenv(
        "BROOK_DATABASE_URL", external or f"sqlite+aiosqlite:///{tmp_path / 'sync.db'}"
    )
    monkeypatch.setenv("BROOK_JWT_SIGNING_KEY", "test-signing-key-at-least-32-bytes-long!")
    config.get_settings.cache_clear()
    db._engine = None
    db._sessionmaker = None

    async def _drop_all() -> None:
        engine = db._engine
        if engine is not None:
            async with engine.begin() as conn:
                await conn.run_sync(Base.metadata.drop_all)
            await engine.dispose()

    with TestClient(create_app()) as tc:
        yield tc
        tc.portal.call(_drop_all)
    db._engine = None
    db._sessionmaker = None
    config.get_settings.cache_clear()
