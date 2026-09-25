"""Admin user management: list users, reset another user's password.

Account creation stays at ``POST /auth/register`` (admin-only after bootstrap).
Everything here requires a global admin.
"""

from __future__ import annotations

import uuid
from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException, Query, Request, status
from sqlalchemy import select
from sqlalchemy.ext.asyncio import AsyncSession

from ..db import get_session
from ..deps import require_admin
from ..models import User
from ..ratelimit import AuthLimiter, client_ip, enforce, get_limiter
from ..schemas import AdminPasswordIn, UserOut
from ..security import hash_password, verify_password
from .auth import lock_user, sign_out_everywhere
from .ws import revoke_sessions

router = APIRouter(prefix="/users", tags=["users"])


@router.get("", response_model=list[UserOut])
async def list_users(
    _admin: Annotated[User, Depends(require_admin)],
    session: Annotated[AsyncSession, Depends(get_session)],
    handle: Annotated[str | None, Query(max_length=64)] = None,
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
    request: Request,
    admin: Annotated[User, Depends(require_admin)],
    session: Annotated[AsyncSession, Depends(get_session)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> None:
    """Set another user's password and sign them out of every device.

    Refused for the admin's own account (400) and for any other admin (403):
    admin passwords only change through ``POST /auth/password``, which re-checks
    the current password, so a stolen admin access token cannot take over an
    admin account.
    """
    # Re-authentication: a stolen admin access token alone must not be able to
    # reset member passwords (that would give the thief persistent logins that
    # outlive the token). Same rule as the TOTP reset (encryption spec §7.6).
    ip = client_ip(request.client.host if request.client else None)
    enforce(limiter, ip, admin.handle)  # the admin_password check is a guessing surface too
    if admin.password_hash is None or not verify_password(admin.password_hash, body.admin_password):
        limiter.failure(ip, admin.handle)
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail={"code": "auth.invalid_credentials", "message": "Admin password is wrong"},
        )
    limiter.success(ip, admin.handle)
    if user_id == admin.id:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"code": "invalid", "message": "Use POST /api/v1/auth/password for your own"},
        )
    target = await lock_user(session, user_id)  # serialise with the target's refreshes
    if target is None:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail={"code": "not_found", "message": "No such user"},
        )
    if target.global_role == "admin":
        # Otherwise one stolen admin token resets a second admin, logs in as them
        # and resets the first: persistent takeover of every admin. Admins change
        # their own password (which re-checks the current one), nothing else.
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail={"code": "authz.forbidden", "message": "Admins change their own password"},
        )
    target.password_hash = hash_password(body.new_password)
    # Always a full sign-out: an admin reset is how a lost or stolen device is cut off.
    cutoff_ms = await sign_out_everywhere(session, target)
    await session.commit()
    await revoke_sessions(target.id, cutoff_ms)
