"""Pydantic request/response models for the API."""

from __future__ import annotations

import uuid
from datetime import datetime

from pydantic import BaseModel, ConfigDict, Field, model_validator


class RegisterIn(BaseModel):
    """Registration payload."""

    handle: str = Field(min_length=2, max_length=64, pattern=r"^[a-zA-Z0-9_.-]+$")
    display_name: str = Field(min_length=1, max_length=128)
    password: str = Field(min_length=8, max_length=256)


class PasswordChangeIn(BaseModel):
    """Change the caller's own password (re-authenticates with the current one)."""

    # No min length on the current password: it is checked against the hash, and
    # accounts may predate the policy. The cap bounds argon2 work per request.
    current_password: str = Field(max_length=256)
    new_password: str = Field(min_length=8, max_length=256)
    # The "Sign out of other devices" checkbox, on by default: revoke every other
    # session at once (refresh tokens, access tokens, open sockets). Off keeps the
    # other devices signed in, e.g. a routine change on a trusted set of devices.
    sign_out_other_devices: bool = True


class AdminPasswordIn(BaseModel):
    """An admin sets another user's password, re-authenticating with their own."""

    admin_password: str = Field(max_length=256)
    new_password: str = Field(min_length=8, max_length=256)


class LoginIn(BaseModel):
    """Login payload. Bounded like RegisterIn: oversized input is a 422 before the
    database, the rate limiter (which keys on the handle) or Argon2 sees it."""

    handle: str = Field(max_length=64)
    password: str = Field(max_length=256)
    # The client can do the TOTP step. A client that predates TOTP doesn't send it
    # and gets 403 auth.totp_client_required for a TOTP user, instead of a 200 it
    # would misread as a login without tokens (spec 2026-09-25-totp §2.1).
    supports_totp: bool = False


class TotpRequiredOut(BaseModel):
    """Login answer for a TOTP user: the password was right; now the code."""

    totp_required: bool = True
    totp_token: str
    expires_in: int


class TotpStepIn(BaseModel):
    """``POST /auth/totp``: exactly one of ``code`` or ``recovery_code``."""

    totp_token: str = Field(max_length=2048)
    code: str | None = Field(default=None, max_length=16)
    recovery_code: str | None = Field(default=None, max_length=64)

    @model_validator(mode="after")
    def _exactly_one(self) -> TotpStepIn:
        if (self.code is None) == (self.recovery_code is None):
            raise ValueError("send exactly one of code or recovery_code")
        return self


class SecondFactorIn(BaseModel):
    """Re-authentication for disabling TOTP or regenerating recovery codes:
    the password plus exactly one of a code or a recovery code."""

    password: str = Field(max_length=256)
    # A TOTP code or a recovery code (iiii-xxxx-xxxx-xxxx-xxxx, 24 chars): spec §2.3.
    code: str | None = Field(default=None, max_length=64)
    recovery_code: str | None = Field(default=None, max_length=64)

    @model_validator(mode="after")
    def _exactly_one(self) -> SecondFactorIn:
        if (self.code is None) == (self.recovery_code is None):
            raise ValueError("send exactly one of code or recovery_code")
        return self


class PasswordIn(BaseModel):
    password: str = Field(max_length=256)


class CodeIn(BaseModel):
    code: str = Field(max_length=16)


class TotpEnrollOut(BaseModel):
    otpauth_uri: str
    expires_in: int


class RecoveryCodesOut(BaseModel):
    recovery_codes: list[str]


class AdminReauthIn(BaseModel):
    admin_password: str = Field(max_length=256)


class RefreshIn(BaseModel):
    """Refresh payload."""

    refresh_token: str = Field(max_length=512)


class TokenPair(BaseModel):
    """Issued session tokens."""

    access_token: str
    refresh_token: str
    token_type: str = "bearer"  # noqa: S105 - field name trips the secret heuristic; not a secret


class TotpLoginOut(TokenPair):
    """``POST /auth/totp``: the pair; with a recovery code, how many are left."""

    recovery_codes_left: int | None = None


class TotpActivateOut(TokenPair):
    """Activation signs out every other session, so it carries this device's new pair."""

    recovery_codes: list[str]


class PasswordChangeOut(TokenPair):
    """``POST /auth/password``: the new pair, plus what the server actually did.

    The protocol has no capability signal, so a client cannot know whether a
    server understood ``sign_out_other_devices``. Echoing the outcome lets it
    word its message from what happened; a missing field means an older server
    (which always signed out the other devices' refresh tokens)."""

    other_devices_signed_out: bool


class UserOut(BaseModel):
    """Public representation of a user."""

    model_config = ConfigDict(from_attributes=True)

    id: uuid.UUID
    handle: str
    display_name: str
    global_role: str
    status: str
    created_at: datetime


class MeOut(UserOut):
    """``GET /auth/me``: the user plus their second-factor state."""

    totp_enabled: bool = False
    recovery_codes_left: int | None = None


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
    public: bool = False


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
    public: bool = False
    archived: bool = False
    # Change sequence (sync spec §3): the cache keeps the highest per row.
    seq: int = 0


class ChannelPatch(BaseModel):
    """Rename/retopic/archive a channel (owner or admin). Omitted fields unchanged."""

    name: str | None = Field(default=None, max_length=128)
    topic: str | None = Field(default=None, max_length=512)
    archived: bool | None = None


class ReadIn(BaseModel):
    """Mark a channel read up to ``message_id`` (or its latest message if omitted)."""

    message_id: uuid.UUID | None = None


class MemberAdd(BaseModel):
    """Add a member to a channel by handle."""

    handle: str = Field(min_length=2, max_length=64)


class FileCreate(BaseModel):
    """Start an attachment upload (attachments spec §3)."""

    filename: str = Field(min_length=1, max_length=1024)
    size: int = Field(ge=1)
    content_type: str = Field(min_length=1, max_length=255)
    # Idempotent like messages: a retry after a lost response returns the same file.
    client_id: uuid.UUID | None = None


class FileOut(BaseModel):
    """An attachment. ``filename`` is the sanitised ASCII name clients save under;
    ``original_name`` is display text only, never a filesystem name."""

    model_config = ConfigDict(from_attributes=True)

    id: uuid.UUID
    channel_id: uuid.UUID
    uploader_id: uuid.UUID
    filename: str
    original_name: str
    size: int
    content_type: str
    status: str
    sha256: str | None
    created_at: datetime


class FileCreated(BaseModel):
    file: FileOut
    upload_url: str


class MessageCreate(BaseModel):
    """Send a message into a channel."""

    # May be empty when files are attached (a photo without a caption); a message needs
    # text or files, checked below.
    body: str = Field(default="", max_length=4000)
    reply_to_id: uuid.UUID | None = None
    # Outbox idempotency: a UUID the client generates once per message. Resending
    # with the same one returns the stored message (200), never a duplicate.
    client_id: uuid.UUID | None = None
    # Committed files to attach (attachments spec §3): uploaded by the author, to this
    # channel, not attached yet.
    attachments: list[uuid.UUID] = Field(default_factory=list, max_length=10)

    @model_validator(mode="after")
    def _text_or_files(self) -> MessageCreate:
        if not self.body.strip() and not self.attachments:
            raise ValueError("A message needs text or at least one attachment")
        return self


class MessageEdit(BaseModel):
    """Edit a message's body. May be empty only on a message with files (the route
    checks: the same "text or files" rule as a send)."""

    body: str = Field(default="", max_length=4000)


class ReplyExcerpt(BaseModel):
    """A compact preview of the message a reply quotes."""

    id: uuid.UUID
    author_handle: str | None
    author_display_name: str | None
    body: str  # truncated for display


class ReactionToggle(BaseModel):
    """Toggle the caller's reaction with this emoji on a message."""

    emoji: str = Field(min_length=1, max_length=32)


class ReactionSummary(BaseModel):
    """An emoji's reaction tally on a message, plus whether the caller reacted."""

    emoji: str
    count: int
    me: bool


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
    # Set on a tombstone (body is then empty). History omits deleted messages; this
    # shows up where a deleted one is still returned (an outbox resend; later /sync).
    deleted_at: datetime | None = None
    reply_to_id: uuid.UUID | None = None
    reply_to: ReplyExcerpt | None = None
    reactions: list[ReactionSummary] = Field(default_factory=list)
    # Echoed so the sender's cache matches its pending outbox entry to this message.
    client_id: uuid.UUID | None = None
    # Change sequence (sync spec §3): the cache keeps the highest per row.
    seq: int = 0
    attachments: list[FileOut] = Field(default_factory=list)
    # Specific @handle mentions resolved to member ids (set only on the live send).
    mentions: list[uuid.UUID] = Field(default_factory=list)
    # True when @channel / @here mentioned everyone (avoids listing all member ids).
    mention_everyone: bool = False
