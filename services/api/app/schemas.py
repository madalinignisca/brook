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
