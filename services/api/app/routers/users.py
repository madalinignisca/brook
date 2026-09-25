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
from ..events import record_event
from ..models import User, utcnow
from ..ratelimit import AuthLimiter, client_ip, enforce, get_limiter
from ..schemas import AdminPasswordIn, AdminReauthIn, UserOut
from ..security import hash_password, verify_password
from .auth import lock_user, sign_out_everywhere
from .totp import remove_totp
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


async def _admin_target(
    user_id: uuid.UUID,
    admin_password: str,
    request: Request,
    admin: User,
    session: AsyncSession,
    limiter: AuthLimiter,
    *,
    what: str,
) -> User:
    """The shared rules of every admin reset (password, TOTP), returning the locked
    target: the admin re-authenticates (rate-limited, since it is a guessing surface
    too), never on themselves (400), never on another admin (403).

    Re-authentication: a stolen admin access token alone must not be able to reset
    other users (that would give the thief persistent logins that outlive the
    token); encryption spec §7.6. No admin targets: otherwise one stolen admin token
    resets a second admin, logs in as them and resets the first, taking over every
    admin. Admins manage their own password and TOTP, which re-check them.
    """
    ip = client_ip(request.client.host if request.client else None)
    enforce(limiter, ip, admin.handle)
    if admin.password_hash is None or not verify_password(admin.password_hash, admin_password):
        limiter.failure(ip, admin.handle)
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail={"code": "auth.invalid_credentials", "message": "Admin password is wrong"},
        )
    limiter.success(ip, admin.handle)
    if user_id == admin.id:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail={"code": "invalid", "message": f"Use your own {what} settings"},
        )
    target = await lock_user(session, user_id)  # serialise with the target's refreshes
    if target is None:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail={"code": "not_found", "message": "No such user"},
        )
    if target.global_role == "admin":
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail={"code": "authz.forbidden", "message": f"Admins manage their own {what}"},
        )
    return target


@router.post("/{user_id}/password", status_code=status.HTTP_204_NO_CONTENT)
async def reset_password(
    user_id: uuid.UUID,
    body: AdminPasswordIn,
    request: Request,
    admin: Annotated[User, Depends(require_admin)],
    session: Annotated[AsyncSession, Depends(get_session)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> None:
    """Set a member's password and sign them out of every device (_admin_target has
    the rules). Admin passwords only change through ``POST /auth/password``."""
    target = await _admin_target(
        user_id, body.admin_password, request, admin, session, limiter, what="password"
    )
    target.password_hash = hash_password(body.new_password)
    target.password_changed_at = utcnow()  # kills a TOTP pending token of the old password
    limiter.code_reset(target.handle)
    record_event(session, target.id, "password_reset", actor_id=admin.id)
    # Always a full sign-out: an admin reset is how a lost or stolen device is cut off.
    cutoff_ms = await sign_out_everywhere(session, target)
    await session.commit()
    await revoke_sessions(target.id, cutoff_ms)


@router.post("/{user_id}/totp/reset", status_code=status.HTTP_204_NO_CONTENT)
async def reset_totp(
    user_id: uuid.UUID,
    body: AdminReauthIn,
    request: Request,
    admin: Annotated[User, Depends(require_admin)],
    session: Annotated[AsyncSession, Depends(get_session)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> None:
    """Remove a member's TOTP and recovery codes (lost phone and codes) and sign them
    out everywhere; they sign in with the password and may enrol again. Idempotent:
    a member without TOTP is a 204 too. Recorded with the admin as actor."""
    target = await _admin_target(
        user_id, body.admin_password, request, admin, session, limiter, what="two-factor"
    )
    await remove_totp(session, target.id)
    limiter.code_reset(target.handle)
    record_event(session, target.id, "totp_reset", actor_id=admin.id)
    cutoff_ms = await sign_out_everywhere(session, target)
    await session.commit()
    await revoke_sessions(target.id, cutoff_ms)
