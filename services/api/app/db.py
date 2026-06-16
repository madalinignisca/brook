"""Async database engine, session factory, and FastAPI dependency."""

from __future__ import annotations

from collections.abc import AsyncIterator
from typing import Any

from sqlalchemy import event
from sqlalchemy.ext.asyncio import (
    AsyncEngine,
    AsyncSession,
    async_sessionmaker,
    create_async_engine,
)

from .config import get_settings
from .models import Base

_engine: AsyncEngine | None = None
_sessionmaker: async_sessionmaker[AsyncSession] | None = None


def _enable_sqlite_fk(engine: AsyncEngine) -> None:
    """Turn on SQLite foreign-key enforcement for dev/test parity with Postgres."""
    if engine.dialect.name != "sqlite":
        return

    @event.listens_for(engine.sync_engine, "connect")
    def _set_pragma(dbapi_conn: Any, _record: Any) -> None:  # noqa: ANN401
        cur = dbapi_conn.cursor()
        cur.execute("PRAGMA foreign_keys=ON")
        cur.close()


def get_engine() -> AsyncEngine:
    """Lazily create the process-wide async engine."""
    global _engine, _sessionmaker
    if _engine is None:
        _engine = create_async_engine(get_settings().database_url, future=True)
        _enable_sqlite_fk(_engine)
        _sessionmaker = async_sessionmaker(_engine, expire_on_commit=False)
    return _engine


def get_sessionmaker() -> async_sessionmaker[AsyncSession]:
    """Return the session factory, creating the engine if needed."""
    get_engine()
    assert _sessionmaker is not None  # noqa: S101 - invariant after get_engine()
    return _sessionmaker


async def init_models() -> None:
    """Create tables when ``auto_create_schema`` is on (Phase 0 / tests).

    Production uses Alembic migrations and sets ``BROOK_AUTO_CREATE_SCHEMA=0``.
    """
    if not get_settings().auto_create_schema:
        return
    engine = get_engine()
    async with engine.begin() as conn:
        await conn.run_sync(Base.metadata.create_all)


async def get_session() -> AsyncIterator[AsyncSession]:
    """FastAPI dependency yielding a scoped async session."""
    async with get_sessionmaker()() as session:
        yield session
