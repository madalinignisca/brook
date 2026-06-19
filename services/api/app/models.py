"""Database models (SQLAlchemy 2.0 typed ORM).

Phase 0 scope: just what local auth needs. The full schema (channels, messages,
files, bots, ...) follows the spec in docs/DATA_MODEL.md.
"""

from __future__ import annotations

import os
import time
import uuid
from datetime import UTC, datetime

from sqlalchemy import Boolean, DateTime, ForeignKey, Index, String, Text
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column


def utcnow() -> datetime:
    """Timezone-aware current UTC time."""
    return datetime.now(UTC)


def uuid7() -> uuid.UUID:
    """A UUIDv7: 48-bit big-endian millisecond timestamp + random, so ids are
    time-sortable. Drives message history pagination (``before=<id>``) without a
    separate ordering column. (stdlib ``uuid`` has no v7 before Python 3.14.)"""
    ms = int(time.time() * 1000)
    data = bytearray(ms.to_bytes(6, "big") + os.urandom(10))
    data[6] = (data[6] & 0x0F) | 0x70  # version 7
    data[8] = (data[8] & 0x3F) | 0x80  # RFC 4122 variant
    return uuid.UUID(bytes=bytes(data))


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


class Channel(Base):
    """A conversation: a named ``channel`` or a 1:1 ``dm``.

    A DM is just a ``kind='dm'`` channel with exactly two members (see
    docs/DATA_MODEL.md). ``name``/``topic`` are unused for DMs.
    """

    __tablename__ = "channels"

    id: Mapped[uuid.UUID] = mapped_column(primary_key=True, default=uuid.uuid4)
    kind: Mapped[str] = mapped_column(String(16))  # 'dm' | 'channel'
    name: Mapped[str | None] = mapped_column(String(128), default=None)
    topic: Mapped[str | None] = mapped_column(String(512), default=None)
    # Nullable to match ondelete=SET NULL: a deleted creator nulls this but the
    # channel (and its history) survives.
    created_by: Mapped[uuid.UUID | None] = mapped_column(
        ForeignKey("users.id", ondelete="SET NULL")
    )
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)


class Membership(Base):
    """A user's membership in a channel. PK = (channel_id, user_id)."""

    __tablename__ = "memberships"

    channel_id: Mapped[uuid.UUID] = mapped_column(
        ForeignKey("channels.id", ondelete="CASCADE"), primary_key=True
    )
    user_id: Mapped[uuid.UUID] = mapped_column(
        ForeignKey("users.id", ondelete="CASCADE"), primary_key=True, index=True
    )
    role: Mapped[str] = mapped_column(String(16), default="member")  # 'owner' | 'member'
    joined_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)
    # Highest message id the user has read in this channel (UUIDv7 is sortable, so
    # unread = messages with a greater id). No FK: messages may be soft-deleted.
    last_read_message_id: Mapped[uuid.UUID | None] = mapped_column(default=None)


class Message(Base):
    """A message in a channel. ``id`` is UUIDv7 (time-sortable → pagination).

    ``author_type`` distinguishes user vs bot (bots are first-class later); for
    Phase 1 the author is always a user. Soft-deleted via ``deleted_at``.
    """

    __tablename__ = "messages"
    __table_args__ = (Index("ix_messages_channel_id_id", "channel_id", "id"),)

    id: Mapped[uuid.UUID] = mapped_column(primary_key=True, default=uuid7)
    channel_id: Mapped[uuid.UUID] = mapped_column(ForeignKey("channels.id", ondelete="CASCADE"))
    author_type: Mapped[str] = mapped_column(String(8), default="user")  # 'user' | 'bot'
    author_id: Mapped[uuid.UUID] = mapped_column()  # polymorphic (user|bot); no FK
    body: Mapped[str] = mapped_column(Text)
    # Quote-reply: the message this one replies to (same channel). SET NULL on
    # delete so a reply survives the quoted message being removed.
    reply_to_id: Mapped[uuid.UUID | None] = mapped_column(
        ForeignKey("messages.id", ondelete="SET NULL"), default=None
    )
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)
    edited_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True), default=None)
    deleted_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True), default=None)
