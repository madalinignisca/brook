"""Alembic migration tests.

Regression cover for the friends-review HIGH finding: the baseline migration must
*adopt* a pre-Alembic database (tables already created by the old ``create_all``
path, no ``alembic_version`` row) instead of crashing with "table already exists".
Also covers the fresh-install and downgrade/upgrade round-trip paths.

These are sync tests on purpose: Alembic drives the async engine via
``asyncio.run`` internally, which cannot be nested inside a running event loop.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

import pytest
from sqlalchemy import create_engine

from alembic import command
from alembic.config import Config
from app import config, db
from app.models import Base

API_DIR = Path(__file__).resolve().parents[1]
BASELINE_REVISION = "8451a806bdec"


def _alembic_config(db_url: str, monkeypatch: pytest.MonkeyPatch) -> Config:
    """An Alembic config pointed at ``db_url`` via the app's settings.

    A bare ``Config()`` (no .ini) is used so env.py skips ``fileConfig`` and does
    not reconfigure global logging for the rest of the suite. env.py still reads
    the URL from settings, so clear the cache after setting the env var.
    """
    monkeypatch.setenv("BROOK_DATABASE_URL", db_url)
    config.get_settings.cache_clear()
    db._engine = None
    db._sessionmaker = None
    cfg = Config()
    cfg.set_main_option("script_location", str(API_DIR / "alembic"))
    return cfg


def _table_names(sqlite_path: Path) -> set[str]:
    with sqlite3.connect(sqlite_path) as conn:
        return {r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")}


def test_upgrade_builds_schema_on_fresh_db(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    db_path = tmp_path / "fresh.db"
    cfg = _alembic_config(f"sqlite+aiosqlite:///{db_path}", monkeypatch)
    command.upgrade(cfg, "head")
    assert {"users", "refresh_tokens", "alembic_version"} <= _table_names(db_path)


def test_upgrade_adopts_pre_alembic_db(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    # Simulate a pre-Alembic deployment: schema built by create_all, no version row.
    db_path = tmp_path / "legacy.db"
    sync_engine = create_engine(f"sqlite:///{db_path}")
    Base.metadata.create_all(sync_engine)
    sync_engine.dispose()
    assert "alembic_version" not in _table_names(db_path)

    cfg = _alembic_config(f"sqlite+aiosqlite:///{db_path}", monkeypatch)
    command.upgrade(cfg, "head")  # must NOT raise "table already exists"

    assert {"users", "refresh_tokens", "alembic_version"} <= _table_names(db_path)
    with sqlite3.connect(db_path) as conn:
        (version,) = conn.execute("SELECT version_num FROM alembic_version").fetchone()
    assert version == BASELINE_REVISION


def test_downgrade_then_upgrade_round_trip(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    db_path = tmp_path / "roundtrip.db"
    cfg = _alembic_config(f"sqlite+aiosqlite:///{db_path}", monkeypatch)
    command.upgrade(cfg, "head")
    command.downgrade(cfg, "base")
    assert "users" not in _table_names(db_path)
    command.upgrade(cfg, "head")
    assert {"users", "refresh_tokens"} <= _table_names(db_path)
