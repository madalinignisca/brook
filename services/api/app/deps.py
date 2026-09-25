"""Shared FastAPI dependencies (current user, admin guard)."""

from __future__ import annotations

import uuid
from typing import Annotated

import jwt
from fastapi import Depends, HTTPException, status
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from sqlalchemy.ext.asyncio import AsyncSession

from .config import Settings, get_settings
from .db import get_session
from .models import User

# auto_error=False so a *missing* Authorization header is handled here as a 401
# `auth.unauthorized` (not authenticated) rather than HTTPBearer's framework 403,
# which would misclassify an unauthenticated caller as an authorization failure.
_bearer = HTTPBearer(auto_error=False)


async def user_from_access_token(
    session: AsyncSession, settings: Settings, token: str
) -> User | None:
    """The one place a REST bearer token becomes a user: a valid, unexpired
    ``access`` JWT of an active user, not revoked by a sign-out-everywhere.
    ``None`` otherwise.

    Every REST path that trusts a bearer token goes through here. A second,
    hand-rolled copy (register's optional caller) once skipped the revocation
    check, letting a stolen admin token keep creating accounts after the admin
    had signed out everywhere. The WebSocket keeps its own variant only because
    it must tell an expired token apart from a bad one (routers/ws.py).
    """
    from .security import decode_access_token, issued_at_ms  # local import avoids cycle

    try:
        payload = decode_access_token(settings, token)
        if payload.get("type") != "access":
            return None
        user_id = uuid.UUID(str(payload["sub"]))
        issued_ms = issued_at_ms(payload)
    except (jwt.PyJWTError, KeyError, ValueError, TypeError):
        return None
    user = await session.get(User, user_id)
    if user is None or user.status != "active" or user.session_revoked(issued_ms):
        return None
    return user


async def get_current_user(
    credentials: Annotated[HTTPAuthorizationCredentials | None, Depends(_bearer)],
    session: Annotated[AsyncSession, Depends(get_session)],
    settings: Annotated[Settings, Depends(get_settings)],
) -> User:
    """Resolve the authenticated, active user from a Bearer access token."""
    if credentials is None:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail={"code": "auth.unauthorized", "message": "Not authenticated"},
            headers={"WWW-Authenticate": "Bearer"},
        )
    user = await user_from_access_token(session, settings, credentials.credentials)
    if user is None:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail={"code": "auth.invalid_token", "message": "Invalid or expired token"},
            headers={"WWW-Authenticate": "Bearer"},
        )
    return user


async def require_admin(user: Annotated[User, Depends(get_current_user)]) -> User:
    """Allow only global admins."""
    if user.global_role != "admin":
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail={"code": "authz.forbidden", "message": "Admin role required"},
        )
    return user
