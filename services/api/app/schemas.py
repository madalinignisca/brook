"""Pydantic request/response models for the API."""

from __future__ import annotations

import uuid
from datetime import datetime

from pydantic import BaseModel, ConfigDict, Field


class RegisterIn(BaseModel):
    """Registration payload."""

    handle: str = Field(min_length=2, max_length=64, pattern=r"^[a-zA-Z0-9_.-]+$")
    display_name: str = Field(min_length=1, max_length=128)
    password: str = Field(min_length=8, max_length=256)


class LoginIn(BaseModel):
    """Login payload."""

    handle: str
    password: str


class RefreshIn(BaseModel):
    """Refresh payload."""

    refresh_token: str


class TokenPair(BaseModel):
    """Issued session tokens."""

    access_token: str
    refresh_token: str
    token_type: str = "bearer"  # noqa: S105 - field name trips the secret heuristic; not a secret


class UserOut(BaseModel):
    """Public representation of a user."""

    model_config = ConfigDict(from_attributes=True)

    id: uuid.UUID
    handle: str
    display_name: str
    global_role: str
    status: str
    created_at: datetime


class UserSummary(BaseModel):
    """Lightweight user reference embedded in channels/messages."""

    model_config = ConfigDict(from_attributes=True)

    id: uuid.UUID
    handle: str
    display_name: str


class ChannelCreate(BaseModel):
    """Create a channel (admin-only) or open a 1:1 DM.

    For ``kind='dm'`` supply ``member`` (the other user's handle); ``name``/
    ``topic`` are ignored. For ``kind='channel'`` supply ``name``.
    """

    kind: str = Field(pattern="^(dm|channel)$")
    name: str | None = Field(default=None, max_length=128)
    topic: str | None = Field(default=None, max_length=512)
    member: str | None = Field(default=None, max_length=64)


class ChannelOut(BaseModel):
    """A channel/DM the caller belongs to, with its members."""

    id: uuid.UUID
    kind: str
    name: str | None
    topic: str | None
    created_by: uuid.UUID | None
    created_at: datetime
    members: list[UserSummary]
    unread_count: int = 0


class ReadIn(BaseModel):
    """Mark a channel read up to ``message_id`` (or its latest message if omitted)."""

    message_id: uuid.UUID | None = None


class MemberAdd(BaseModel):
    """Add a member to a channel by handle."""

    handle: str = Field(min_length=2, max_length=64)


class MessageCreate(BaseModel):
    """Send a message into a channel."""

    body: str = Field(min_length=1, max_length=4000)
    reply_to_id: uuid.UUID | None = None


class MessageEdit(BaseModel):
    """Edit a message's body."""

    body: str = Field(min_length=1, max_length=4000)


class ReplyExcerpt(BaseModel):
    """A compact preview of the message a reply quotes."""

    id: uuid.UUID
    author_handle: str | None
    author_display_name: str | None
    body: str  # truncated for display


class MessageOut(BaseModel):
    """A persisted message, with its author resolved for display."""

    id: uuid.UUID
    channel_id: uuid.UUID
    author_id: uuid.UUID
    author_handle: str | None
    author_display_name: str | None
    body: str
    created_at: datetime
    edited_at: datetime | None
    reply_to_id: uuid.UUID | None = None
    reply_to: ReplyExcerpt | None = None
