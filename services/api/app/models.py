"""Database models (SQLAlchemy 2.0 typed ORM).

Phase 0 scope: just what local auth needs. The full schema (channels, messages,
files, bots, ...) follows the spec in docs/DATA_MODEL.md.
"""

from __future__ import annotations

import os
import time
import uuid
from datetime import UTC, datetime

from sqlalchemy import (
    BigInteger,
    Boolean,
    DateTime,
    ForeignKey,
    Index,
    String,
    Text,
    UniqueConstraint,
    text,
)
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
    # Sync change sequence (sync spec §2), stamped automatically by app/sync.py.
    seq: Mapped[int] = mapped_column(BigInteger, default=0, index=True)
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)
    # "Sign out everywhere": access tokens issued before this instant are refused
    # on REST and WebSocket, so a password change or admin reset takes effect in
    # seconds instead of when the stateless 15-minute access token expires.
    # None = never revoked. Compared in milliseconds (see security.issued_at_ms).
    sessions_valid_after: Mapped[datetime | None] = mapped_column(
        DateTime(timezone=True), default=None
    )

    # When the password last changed (self change or admin reset). A TOTP pending
    # token minted by proving an older password dies with it, even when the change
    # left other sessions signed in (spec 2026-09-25-totp §2.2).
    password_changed_at: Mapped[datetime | None] = mapped_column(
        DateTime(timezone=True), default=None
    )

    def password_changed_since(self, issued_at_ms: int) -> bool:
        """True if the password changed after a token issued at ``issued_at_ms``."""
        if self.password_changed_at is None:
            return False
        return issued_at_ms < int(ensure_utc(self.password_changed_at).timestamp() * 1000)

    def session_revoked(self, issued_at_ms: int) -> bool:
        """True if a token issued at ``issued_at_ms`` predates a sign-out-everywhere."""
        if self.sessions_valid_after is None:
            return False
        cutoff_ms = int(ensure_utc(self.sessions_valid_after).timestamp() * 1000)
        return issued_at_ms < cutoff_ms


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
    # One login's chain of rotations (one device). Reuse of a rotated token revokes
    # this family only, never the user's other devices (routers/auth.py `_rotate`).
    family_id: Mapped[uuid.UUID] = mapped_column(default=uuid.uuid4, index=True)
    # Set when rotated (as opposed to revoked by logout or sign-out): only a rotated
    # token presented again is evidence of theft, or of a client that crashed before
    # saving its successor.
    rotated_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True), default=None)
    # No foreign key on purpose: rotation writes this on the old row before the
    # successor row is inserted (same transaction), which a non-deferred FK refuses.
    replaced_by_id: Mapped[uuid.UUID | None] = mapped_column(default=None)


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
    # Sync change sequence (sync spec §2), stamped automatically by app/sync.py.
    seq: Mapped[int] = mapped_column(BigInteger, default=0, index=True)
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)
    # Public channels are browsable + self-joinable by any user (vs invite-only).
    public: Mapped[bool] = mapped_column(default=False)
    # Archived channels are read-only and hidden from the default list.
    archived_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True), default=None)


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
    # Sync change sequence (sync spec §2), stamped automatically by app/sync.py.
    seq: Mapped[int] = mapped_column(BigInteger, default=0, index=True)
    # The seq of the insert only (seq above moves with every read-marker update, so it
    # can't tell /sync that a channel is new to this member).
    joined_seq: Mapped[int] = mapped_column(BigInteger, default=0)
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
    __table_args__ = (
        Index("ix_messages_channel_id_id", "channel_id", "id"),
        # Outbox idempotency (sync spec §4): one message per (author, client_id).
        Index(
            "uq_messages_author_client_id",
            "author_id",
            "client_id",
            unique=True,
            postgresql_where=text("client_id IS NOT NULL"),
            sqlite_where=text("client_id IS NOT NULL"),
        ),
    )

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
    # Generated by the sending client; a retry with the same id returns the stored
    # message instead of storing it twice (the outbox may resend after a lost reply).
    client_id: Mapped[uuid.UUID | None] = mapped_column(default=None)
    # Sync change sequence (sync spec §2), stamped automatically by app/sync.py.
    seq: Mapped[int] = mapped_column(BigInteger, default=0, index=True)
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)
    edited_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True), default=None)
    deleted_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True), default=None)


class Reaction(Base):
    """An emoji reaction by one user on one message (composite PK = uniqueness)."""

    __tablename__ = "reactions"

    message_id: Mapped[uuid.UUID] = mapped_column(
        ForeignKey("messages.id", ondelete="CASCADE"), primary_key=True
    )
    user_id: Mapped[uuid.UUID] = mapped_column(
        ForeignKey("users.id", ondelete="CASCADE"), primary_key=True
    )
    emoji: Mapped[str] = mapped_column(String(32), primary_key=True)
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)


class Totp(Base):
    """A user's TOTP secret (spec 2026-09-25-totp §3). One row per user; a new row
    (new ``id``) per enrolment, so a re-enrolled secret never reuses an AAD."""

    __tablename__ = "totp"

    id: Mapped[uuid.UUID] = mapped_column(primary_key=True, default=uuid.uuid4)
    user_id: Mapped[uuid.UUID] = mapped_column(
        ForeignKey("users.id", ondelete="CASCADE"), unique=True
    )
    # SecretBox ciphertext (purpose TOTP_SECRET, row_pk = id). Never plaintext.
    secret: Mapped[str] = mapped_column(Text)
    activated_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True), default=None)
    pending_expires_at: Mapped[datetime | None] = mapped_column(
        DateTime(timezone=True), default=None
    )
    # Replay guard (RFC 6238 §5.2): a code for a step <= this is refused everywhere.
    last_used_step: Mapped[int | None] = mapped_column(BigInteger, default=None)
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)


class RecoveryCode(Base):
    """One-time recovery code: public ``lookup`` + Argon2id hash of the secret part."""

    __tablename__ = "recovery_codes"
    __table_args__ = (UniqueConstraint("user_id", "lookup"),)

    id: Mapped[uuid.UUID] = mapped_column(primary_key=True, default=uuid.uuid4)
    user_id: Mapped[uuid.UUID] = mapped_column(
        ForeignKey("users.id", ondelete="CASCADE"), index=True
    )
    lookup: Mapped[str] = mapped_column(String(4))
    code_hash: Mapped[str] = mapped_column(Text)
    used_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True), default=None)


class AuthEvent(Base):
    """Append-only record of account-security events (spec §3). No IP, no user agent:
    GDPR minimisation. ``actor_id`` NULL = the user themselves or the host CLI."""

    __tablename__ = "auth_events"

    id: Mapped[uuid.UUID] = mapped_column(primary_key=True, default=uuid.uuid4)
    user_id: Mapped[uuid.UUID] = mapped_column(
        ForeignKey("users.id", ondelete="CASCADE"), index=True
    )
    actor_id: Mapped[uuid.UUID | None] = mapped_column(
        ForeignKey("users.id", ondelete="SET NULL"), default=None
    )
    kind: Mapped[str] = mapped_column(String(40))
    via: Mapped[str] = mapped_column(String(16), default="api")
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)


class File(Base):
    """An attachment (attachments spec §8). The bytes live on the local filesystem at
    ``<BROOK_FILES_DIR>/<id[:2]>/<id>``, never named after the user's filename."""

    __tablename__ = "files"
    __table_args__ = (
        Index("ix_files_status_created", "status", "created_at"),
        Index(
            "uq_files_uploader_client_id",
            "uploader_id",
            "client_id",
            unique=True,
            postgresql_where=text("client_id IS NOT NULL"),
            sqlite_where=text("client_id IS NOT NULL"),
        ),
    )

    id: Mapped[uuid.UUID] = mapped_column(primary_key=True, default=uuid.uuid4)
    channel_id: Mapped[uuid.UUID] = mapped_column(ForeignKey("channels.id", ondelete="CASCADE"))
    uploader_id: Mapped[uuid.UUID] = mapped_column(
        ForeignKey("users.id", ondelete="CASCADE"), index=True
    )
    filename: Mapped[str] = mapped_column(Text)  # sanitised ASCII (app/filenames.py)
    original_name: Mapped[str] = mapped_column(Text)  # display only
    size: Mapped[int] = mapped_column(BigInteger)
    content_type: Mapped[str] = mapped_column(String(255))
    sha256: Mapped[str | None] = mapped_column(String(64), default=None)
    status: Mapped[str] = mapped_column(String(16), default="pending")
    client_id: Mapped[uuid.UUID | None] = mapped_column(default=None)
    message_id: Mapped[uuid.UUID | None] = mapped_column(
        ForeignKey("messages.id", ondelete="SET NULL"), default=None, index=True
    )
    # Where it sits in its message's attachment list, as the sender ordered them (set
    # on attach). Reads order by it, so every route shows the files in the same order
    # as the send's own answer, not in upload order.
    position: Mapped[int | None] = mapped_column(default=None)
    created_at: Mapped[datetime] = mapped_column(DateTime(timezone=True), default=utcnow)
    committed_at: Mapped[datetime | None] = mapped_column(DateTime(timezone=True), default=None)


class SyncCounter(Base):
    """One row: the change sequence (sync spec §2). Its row lock, taken by the first
    change-writing flush of a transaction and held until commit, makes seq order equal
    commit order. ``floor``: cursors below it get 410 sync.reset."""

    __tablename__ = "sync_counter"

    id: Mapped[int] = mapped_column(primary_key=True)
    seq: Mapped[int] = mapped_column(BigInteger, default=1)
    floor: Mapped[int] = mapped_column(BigInteger, default=0)


class SyncTombstone(Base):
    """A membership that ended (left, removed, or its channel deleted), so both the
    removed user (``removed_channels``) and the other members (``left_members``) learn
    of it on their next /sync."""

    __tablename__ = "sync_tombstones"

    id: Mapped[uuid.UUID] = mapped_column(primary_key=True, default=uuid.uuid4)
    channel_id: Mapped[uuid.UUID] = mapped_column(index=True)  # no FK: outlives the channel
    user_id: Mapped[uuid.UUID] = mapped_column(index=True)
    seq: Mapped[int] = mapped_column(BigInteger, index=True)
