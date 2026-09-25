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


async def get_current_user(
    credentials: Annotated[HTTPAuthorizationCredentials | None, Depends(_bearer)],
    session: Annotated[AsyncSession, Depends(get_session)],
    settings: Annotated[Settings, Depends(get_settings)],
) -> User:
    """Resolve the authenticated, active user from a Bearer access token."""
    from .security import decode_access_token, issued_at_ms  # local import avoids cycle

    invalid = HTTPException(
        status_code=status.HTTP_401_UNAUTHORIZED,
        detail={"code": "auth.invalid_token", "message": "Invalid or expired token"},
        headers={"WWW-Authenticate": "Bearer"},
    )
    if credentials is None:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail={"code": "auth.unauthorized", "message": "Not authenticated"},
            headers={"WWW-Authenticate": "Bearer"},
        )
    try:
        payload = decode_access_token(settings, credentials.credentials)
        if payload.get("type") != "access":
            raise invalid
        user_id = uuid.UUID(str(payload["sub"]))
    except (jwt.PyJWTError, KeyError, ValueError):
        raise invalid from None

    user = await session.get(User, user_id)
    if user is None or user.status != "active":
        raise invalid
    if user.session_revoked(issued_at_ms(payload)):
        raise invalid  # signed out everywhere after this token was issued
    return user


async def require_admin(user: Annotated[User, Depends(get_current_user)]) -> User:
    """Allow only global admins."""
    if user.global_role != "admin":
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail={"code": "authz.forbidden", "message": "Admin role required"},
        )
    return user
