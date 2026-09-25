"""Account-security events (``auth_events``, spec 2026-09-25-totp §3).

Append-only. No IP address and no user agent (GDPR minimisation). ``actor_id``
is NULL when the user acted themselves or the host CLI did (``via``)."""

from __future__ import annotations

import uuid

from sqlalchemy.ext.asyncio import AsyncSession

from .models import AuthEvent


def record_event(
    session: AsyncSession,
    user_id: uuid.UUID,
    kind: str,
    *,
    actor_id: uuid.UUID | None = None,
    via: str = "api",
) -> None:
    """Append an event; it commits with the caller's transaction."""
    session.add(AuthEvent(user_id=user_id, kind=kind, actor_id=actor_id, via=via))
