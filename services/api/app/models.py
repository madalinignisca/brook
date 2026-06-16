"""Database models (SQLAlchemy 2.0 typed ORM).

Phase 0 scope: just what local auth needs. The full schema (channels, messages,
files, bots, ...) follows the spec in docs/DATA_MODEL.md.
"""

from __future__ import annotations

import uuid
from datetime import UTC, datetime

from sqlalchemy import Boolean, DateTime, ForeignKey, String
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column


def utcnow() -> datetime:
    """Timezone-aware current UTC time."""
    return datetime.now(UTC)


def ensure_utc(value: datetime) -> datetime:
    """Return a timezone-aware UTC datetime.

    SQLite (dev/tests) drops tzinfo on round-trip while Postgres preserves it;
    normalize so naive values read back from the DB are safely comparable.
    """
    return value if value.tzinfo is not None else value.replace(tzinfo=UTC)


class Base(DeclarativeBase):
    """Declarative base for all models."""


class User(Base):
    """A Brook user.

    ``global_role`` is ``admin`` or ``member`` (see docs/DATA_MODEL.md). The first
    user created bootstraps as ``admin``. ``password_hash`` is nullable to allow
    federated-only (OIDC/LDAP) accounts later; local credentials will move to a
    dedicated table per the spec when federation lands.
    """

    __tablename__ = "users"

    id: Mapped[uuid.UUID] = mapped_column(primary_key=True, default=uuid.uuid4)
    handle: Mapped[str] = mapped_column(String(64), unique=True, index=True)
    display_name: Mapped[str] = mapped_column(String(128))
    password_hash: Mapped[str | None] = mapped_column(String(255), default=None)
    global_role: Mapped[str] = mapped_column(String(16), default="member")
    status: Mapped[str] = mapped_column(String(16), default="active")
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)


class RefreshToken(Base):
    """A persisted, rotatable refresh token (stored only as a hash)."""

    __tablename__ = "refresh_tokens"

    id: Mapped[uuid.UUID] = mapped_column(primary_key=True, default=uuid.uuid4)
    user_id: Mapped[uuid.UUID] = mapped_column(
        ForeignKey("users.id", ondelete="CASCADE"), index=True
    )
    token_hash: Mapped[str] = mapped_column(String(64), unique=True, index=True)
    expires_at: Mapped[datetime] = mapped_column(DateTime(timezone=True))
    revoked: Mapped[bool] = mapped_column(Boolean, default=False)
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)
