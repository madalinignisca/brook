"""Admin user management: list users, reset another user's password.

Account creation stays at ``POST /auth/register`` (admin-only after bootstrap).
Everything here requires a global admin.
"""

from __future__ import annotations

import uuid
from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException, status
from sqlalchemy import select
from sqlalchemy.ext.asyncio import AsyncSession

from ..db import get_session
from ..deps import require_admin
from ..models import User
from ..schemas import AdminPasswordIn, UserOut
from ..security import hash_password
from .auth import revoke_all_refresh_tokens

router = APIRouter(prefix="/users", tags=["users"])


@router.get("", response_model=list[UserOut])
async def list_users(
    _admin: Annotated[User, Depends(require_admin)],
    session: Annotated[AsyncSession, Depends(get_session)],
    handle: str | None = None,
) -> list[User]:
    """All users ordered by handle, or the exact ``handle`` match (404 if none).

    Unpaginated on purpose: this serves a family-sized server's admin sheet.
    """
    query = select(User).order_by(User.handle)
    if handle is not None:
        query = query.where(User.handle == handle)
    users = list((await session.scalars(query)).all())
    if handle is not None and not users:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail={"code": "not_found", "message": "No such user"},
        )
    return users


@router.post("/{user_id}/password", status_code=status.HTTP_204_NO_CONTENT)
async def reset_password(
    user_id: uuid.UUID,
    body: AdminPasswordIn,
    admin: Annotated[User, Depends(require_admin)],
    session: Annotated[AsyncSession, Depends(get_session)],
) -> None:
    """Set another user's password and sign them out of every device.

    Refused for the admin's own account: changing your own password must go
    through ``POST /auth/password``, which re-checks the current password, so a
    stolen admin access token cannot silently take over the admin account.
    """
    if user_id == admin.id:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"code": "invalid", "message": "Use POST /api/v1/auth/password for your own"},
        )
    target = await session.get(User, user_id)
    if target is None:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail={"code": "not_found", "message": "No such user"},
        )
    target.password_hash = hash_password(body.new_password)
    await revoke_all_refresh_tokens(session, target.id)
    await session.commit()
