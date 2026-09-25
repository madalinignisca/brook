"""Local authentication: bootstrap registration, login, refresh, me.

OIDC and LDAP (docs/AUTH.md) arrive in Phase 0b; this is the local-account path.
"""

from __future__ import annotations

import uuid
from datetime import timedelta
from typing import Annotated, Any, cast

import jwt
from fastapi import APIRouter, BackgroundTasks, Depends, HTTPException, Request, status
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from sqlalchemy import func, select, update
from sqlalchemy.engine import CursorResult
from sqlalchemy.ext.asyncio import AsyncSession

from ..config import Settings, get_settings
from ..db import get_session
from ..deps import get_current_user
from ..models import RefreshToken, User, ensure_utc, utcnow
from ..ratelimit import AuthLimiter, client_ip, enforce, get_limiter
from ..schemas import LoginIn, PasswordChangeIn, RefreshIn, RegisterIn, TokenPair, UserOut
from ..security import (
    create_access_token,
    decode_access_token,
    dummy_verify,
    hash_password,
    hash_token,
    needs_rehash,
    new_refresh_token,
    verify_password,
)
from .ws import revoke_sessions

router = APIRouter(prefix="/auth", tags=["auth"])

_optional_bearer = HTTPBearer(auto_error=False)


async def _optional_user(
    credentials: Annotated[HTTPAuthorizationCredentials | None, Depends(_optional_bearer)],
    session: Annotated[AsyncSession, Depends(get_session)],
    settings: Annotated[Settings, Depends(get_settings)],
) -> User | None:
    """Resolve the caller if a valid token is present; else ``None``."""
    if credentials is None:
        return None
    try:
        payload = decode_access_token(settings, credentials.credentials)
        user = await session.get(User, uuid.UUID(str(payload["sub"])))
    except (jwt.PyJWTError, KeyError, ValueError):
        return None
    return user if user and user.status == "active" else None


async def lock_user(session: AsyncSession, user_id: uuid.UUID) -> User | None:
    """Load ``user_id`` with ``SELECT ... FOR UPDATE``, refreshing any cached copy.

    Every path that issues or revokes a user's refresh tokens takes this lock
    first (login, refresh, password change, admin reset), so those transactions
    run one at a time per user. Without it, on Postgres (READ COMMITTED) a
    refresh that commits its new token while a "revoke all" UPDATE is running is
    missed by that UPDATE: the new token was not in its snapshot. That token
    would survive a password change, i.e. an attacker rotating a stolen refresh
    token would stay signed in. SQLite ignores FOR UPDATE; it serialises writers
    anyway. tests/test_token_races.py proves the ordering on Postgres.
    """
    user: User | None = await session.scalar(
        select(User)
        .where(User.id == user_id)
        .with_for_update()
        .execution_options(populate_existing=True)
    )
    return user


async def _issue_tokens(session: AsyncSession, settings: Settings, user: User) -> TokenPair:
    """Create an access token and a persisted refresh token for ``user``."""
    access = create_access_token(settings, user.id, user.global_role)
    raw, token_hash = new_refresh_token()
    session.add(
        RefreshToken(
            user_id=user.id,
            token_hash=token_hash,
            expires_at=utcnow() + timedelta(seconds=settings.refresh_ttl_seconds),
        )
    )
    await session.commit()
    return TokenPair(access_token=access, refresh_token=raw)


@router.post("/register", response_model=UserOut, status_code=status.HTTP_201_CREATED)
async def register(
    body: RegisterIn,
    request: Request,
    session: Annotated[AsyncSession, Depends(get_session)],
    caller: Annotated[User | None, Depends(_optional_user)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> User:
    """Create a user.

    The **first** user bootstraps as global ``admin`` (open). Once any user
    exists, only an authenticated admin may create further accounts.
    """
    ip = client_ip(request.client.host if request.client else None)
    enforce(limiter, ip)
    count = await session.scalar(select(func.count()).select_from(User))
    is_first = (count or 0) == 0
    if not is_first and (caller is None or caller.global_role != "admin"):
        limiter.failure(ip)  # an unauthenticated attempt on a closed endpoint
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail={"code": "authz.forbidden", "message": "Admin role required to add users"},
        )

    exists = await session.scalar(select(User).where(User.handle == body.handle))
    if exists is not None:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail={"code": "conflict", "message": "Handle already taken"},
        )

    limiter.success(ip)
    user = User(
        handle=body.handle,
        display_name=body.display_name,
        password_hash=hash_password(body.password),
        global_role="admin" if is_first else "member",
    )
    session.add(user)
    await session.commit()
    await session.refresh(user)
    return user


@router.post("/login", response_model=TokenPair)
async def login(
    body: LoginIn,
    request: Request,
    session: Annotated[AsyncSession, Depends(get_session)],
    settings: Annotated[Settings, Depends(get_settings)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> TokenPair:
    """Authenticate with handle + password, returning a token pair.

    Rate limited before any Argon2 work (app/ratelimit.py). A 429 is answered the
    same way for known and unknown handles, so it reveals nothing either."""
    ip = client_ip(request.client.host if request.client else None)
    enforce(limiter, ip, body.handle)
    # FOR UPDATE: see lock_user. Also means the hash verified here is the one
    # committed last, so a login racing a password change cannot use the old one.
    user = await session.scalar(select(User).where(User.handle == body.handle).with_for_update())
    bad = HTTPException(
        status_code=status.HTTP_401_UNAUTHORIZED,
        detail={"code": "auth.invalid_credentials", "message": "Invalid handle or password"},
    )
    if user is None or user.password_hash is None or user.status != "active":
        dummy_verify(body.password)  # normalize timing → no user enumeration
        limiter.failure(ip, body.handle)
        raise bad
    if not verify_password(user.password_hash, body.password):
        limiter.failure(ip, body.handle)
        raise bad
    limiter.success(ip, body.handle)
    if needs_rehash(user.password_hash):  # transparently upgrade params on login
        user.password_hash = hash_password(body.password)
    return await _issue_tokens(session, settings, user)


@router.post("/refresh", response_model=TokenPair)
async def refresh(
    body: RefreshIn,
    request: Request,
    session: Annotated[AsyncSession, Depends(get_session)],
    settings: Annotated[Settings, Depends(get_settings)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> TokenPair:
    """Rotate a refresh token: atomically revoke the old one, issue a fresh pair."""
    ip = client_ip(request.client.host if request.client else None)
    enforce(limiter, ip)
    try:
        pair = await _rotate(body, session, settings)
    except _LostRotationRace:
        raise  # two tabs refreshing at once: benign, not a failure
    except HTTPException:
        limiter.failure(ip)
        raise
    limiter.success(ip)
    return pair


class _LostRotationRace(HTTPException):
    """A valid, unrevoked token whose rotation another request won concurrently."""


async def _rotate(body: RefreshIn, session: AsyncSession, settings: Settings) -> TokenPair:
    """The refresh itself; any HTTPException is a failed credential."""
    token = await session.scalar(
        select(RefreshToken).where(RefreshToken.token_hash == hash_token(body.refresh_token))
    )
    bad = HTTPException(
        status_code=status.HTTP_401_UNAUTHORIZED,
        detail={"code": "auth.invalid_token", "message": "Invalid or expired refresh token"},
    )
    if token is None or ensure_utc(token.expires_at) <= utcnow():
        raise bad
    # Lock the user BEFORE the compare-and-set below (see lock_user).
    user = await lock_user(session, token.user_id)
    if user is None or user.status != "active":
        raise bad
    # Read before the UPDATE: the ORM UPDATE's synchronize_session sets
    # token.revoked=True in memory even when it matched no row.
    was_revoked = bool(token.revoked)
    # Compare-and-set: only the first concurrent rotation flips revoked→true, so
    # two simultaneous /refresh calls can't both mint a new token (TOCTOU-safe).
    result = cast(
        "CursorResult[Any]",
        await session.execute(
            update(RefreshToken)
            .where(RefreshToken.id == token.id, RefreshToken.revoked.is_(False))
            .values(revoked=True)
        ),
    )
    if result.rowcount != 1 and not was_revoked:
        # It was unrevoked when read: a concurrent rotation won (two tabs).
        raise _LostRotationRace(status_code=bad.status_code, detail=bad.detail)
    if result.rowcount != 1:
        # Already rotated/revoked, or token reuse.
        # TODO(Phase 0b): treat reuse of a revoked token as theft → revoke the family.
        raise bad
    return await _issue_tokens(session, settings, user)


async def sign_out_everywhere(session: AsyncSession, user: User) -> int:
    """Revoke every session of ``user`` (not committed); returns the cutoff in ms.

    Refresh tokens are revoked, and ``sessions_valid_after`` makes every access
    token issued before now fail at once on REST and WebSocket. After commit the
    caller closes the live sockets with ``ws.revoke_sessions(user.id, cutoff)``.
    A pair issued after this call is on the right side of the cutoff.
    """
    now = utcnow()
    user.sessions_valid_after = now
    await revoke_all_refresh_tokens(session, user.id)
    return int(now.timestamp() * 1000)


async def revoke_all_refresh_tokens(session: AsyncSession, user_id: uuid.UUID) -> None:
    """Revoke every live refresh token of ``user_id`` (signs out all devices).

    Access tokens already issued stay valid until they expire (access_ttl_seconds,
    15 min): they are stateless JWTs. Revoking refresh tokens is what stops a
    device from staying signed in beyond that.
    """
    await session.execute(
        update(RefreshToken)
        .where(RefreshToken.user_id == user_id, RefreshToken.revoked.is_(False))
        .values(revoked=True)
    )


@router.post("/password", response_model=TokenPair)
async def change_password(
    body: PasswordChangeIn,
    request: Request,
    background: BackgroundTasks,
    user: Annotated[User, Depends(get_current_user)],
    session: Annotated[AsyncSession, Depends(get_session)],
    settings: Annotated[Settings, Depends(get_settings)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> TokenPair:
    """Change the caller's password; sign out every device; return a fresh pair.

    Every refresh token of the user is revoked, including the caller's own, and
    the response carries a new pair so this client stays signed in. The old
    refresh token is dead from the moment this commits: a client that loses the
    response is signed out on its next refresh and signs in with the new password.

    A wrong current password is 403, deliberately not 401: clients treat 401 as
    "access token expired" and would refresh-and-retry instead of reporting it.
    """
    # Rate limited like login, before any Argon2 work: otherwise a stolen access
    # token could guess the current password at full speed through this route.
    ip = client_ip(request.client.host if request.client else None)
    enforce(limiter, ip, user.handle)
    # Re-read under the lock (see lock_user): the copy from get_current_user was
    # loaded without it, and a concurrent change may have replaced the hash.
    locked = await lock_user(session, user.id)
    if locked is None or locked.status != "active":  # deleted/disabled since auth
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail={"code": "auth.invalid_token", "message": "Invalid or expired token"},
        )
    user = locked
    if user.password_hash is None:
        dummy_verify(body.current_password)  # same timing as a real check
        ok = False
    else:
        ok = verify_password(user.password_hash, body.current_password)
    if not ok:
        limiter.failure(ip, user.handle)
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail={"code": "auth.invalid_credentials", "message": "Current password is wrong"},
        )
    limiter.success(ip, user.handle)
    if body.new_password == body.current_password:
        # Would sign out every device for no change; almost always a UI slip.
        raise HTTPException(
            status_code=status.HTTP_422_UNPROCESSABLE_ENTITY,
            detail={"code": "invalid", "message": "New password must differ from the current one"},
        )
    user.password_hash = hash_password(body.new_password)
    if not body.sign_out_other_devices:
        return await _issue_tokens(session, settings, user)
    cutoff_ms = await sign_out_everywhere(session, user)
    pair = await _issue_tokens(session, settings, user)  # commits; issued after the cutoff
    # Close the live sockets only AFTER this response is sent. The changing
    # device's own socket is among them; closed first, its client would refresh
    # with the old (now revoked) refresh token before it had the new pair, and
    # sign itself out. Other devices lose at most the time to send this response:
    # their access tokens are already refused on REST and on re-auth.
    background.add_task(revoke_sessions, user.id, cutoff_ms)
    return pair


@router.post("/logout", status_code=status.HTTP_204_NO_CONTENT)
async def logout(
    body: RefreshIn,
    session: Annotated[AsyncSession, Depends(get_session)],
) -> None:
    """Revoke a refresh token (idempotent)."""
    await session.execute(
        update(RefreshToken)
        .where(
            RefreshToken.token_hash == hash_token(body.refresh_token),
            RefreshToken.revoked.is_(False),
        )
        .values(revoked=True)
    )
    await session.commit()


@router.get("/me", response_model=UserOut)
async def me(user: Annotated[User, Depends(get_current_user)]) -> User:
    """Return the currently authenticated user."""
    return user
