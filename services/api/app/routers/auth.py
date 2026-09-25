"""Local authentication: bootstrap registration, login, refresh, me.

OIDC and LDAP (docs/AUTH.md) arrive in Phase 0b; this is the local-account path.
"""

from __future__ import annotations

import uuid
from datetime import datetime, timedelta
from typing import Annotated, Any, cast

from fastapi import APIRouter, BackgroundTasks, Depends, HTTPException, Request, status
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from sqlalchemy import func, select, update
from sqlalchemy.engine import CursorResult
from sqlalchemy.ext.asyncio import AsyncSession

from ..config import Settings, get_settings
from ..db import get_session
from ..deps import get_current_user, user_from_access_token
from ..events import record_event
from ..models import RecoveryCode, RefreshToken, Totp, User, ensure_utc, utcnow
from ..ratelimit import AuthLimiter, client_ip, enforce, get_limiter
from ..schemas import (
    LoginIn,
    MeOut,
    PasswordChangeIn,
    PasswordChangeOut,
    RefreshIn,
    RegisterIn,
    TokenPair,
    TotpRequiredOut,
    UserOut,
)
from ..security import (
    TOTP_PENDING_TTL_S,
    create_access_token,
    create_totp_pending_token,
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
    return await user_from_access_token(session, settings, credentials.credentials)


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


async def _issue_tokens(
    session: AsyncSession,
    settings: Settings,
    user: User,
    *,
    token_id: uuid.UUID | None = None,
    family_id: uuid.UUID | None = None,
) -> TokenPair:
    """Create an access token and a persisted refresh token for ``user``.

    A new login starts a new family; a rotation passes its family (and the id it
    already recorded as the old token's ``replaced_by_id``)."""
    access = create_access_token(settings, user.id, user.global_role)
    raw, token_hash = new_refresh_token()
    session.add(
        RefreshToken(
            id=token_id or uuid.uuid4(),
            family_id=family_id or uuid.uuid4(),
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


@router.post("/login", response_model=TokenPair | TotpRequiredOut)
async def login(
    body: LoginIn,
    request: Request,
    session: Annotated[AsyncSession, Depends(get_session)],
    settings: Annotated[Settings, Depends(get_settings)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> TokenPair | TotpRequiredOut:
    """Authenticate with handle + password, returning a token pair, or for a TOTP
    user a pending token for ``POST /auth/totp`` (spec 2026-09-25-totp §2.1).

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
    if needs_rehash(user.password_hash):  # transparently upgrade params on login
        user.password_hash = hash_password(body.password)
    totp_on = await session.scalar(
        select(Totp.id).where(Totp.user_id == user.id, Totp.activated_at.is_not(None))
    )
    if totp_on is not None:
        # Half a login: no limiter.success here. Success marks this IP trusted for the
        # handle, which exempts it from the TOTP code budget; a password-only attacker
        # must never earn that. It is recorded when /auth/totp completes.
        # Minted while still holding the row lock, so no password change can commit
        # between this token's iat_ms and the lock's release (password_changed_since).
        token, _jti = create_totp_pending_token(settings, user.id)
        await session.commit()  # keep a rehash
        if not body.supports_totp:
            raise HTTPException(
                status_code=status.HTTP_403_FORBIDDEN,
                detail={"code": "auth.totp_client_required", "message": "Update the app"},
            )
        return TotpRequiredOut(totp_token=token, expires_in=TOTP_PENDING_TTL_S)
    limiter.success(ip, body.handle)
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
    if token is None:
        raise bad
    # Lock the user BEFORE the compare-and-set below (see lock_user).
    user = await lock_user(session, token.user_id)
    if user is None or user.status != "active":
        raise bad
    # Re-read under the lock. The row read above predates it: a grace replay by
    # someone else that committed while we waited would otherwise look like a lost
    # two-tab race (a silent, uncounted 401), and the device would sign itself out
    # while the other holder kept the chain. Read now, the device sees its token
    # retired by that grace: a reuse verdict, so the family ends for both holders,
    # recorded.
    await session.refresh(token)
    expired = ensure_utc(token.expires_at) <= utcnow()
    if expired and not token.revoked:
        raise bad
    # Read before the UPDATE: the ORM UPDATE's synchronize_session sets
    # token.revoked=True (and rotated_at, replaced_by_id) in memory even when it
    # matched no row.
    was_revoked = bool(token.revoked)
    rotated_at = token.rotated_at
    replaced_by = token.replaced_by_id
    now = utcnow()
    successor = uuid.uuid4()
    # Compare-and-set: only the first concurrent rotation flips revoked→true, so
    # two simultaneous /refresh calls can't both mint a new token (TOCTOU-safe).
    if not was_revoked and await _cas_rotate(session, token.id, successor, now):
        return await _issue_tokens(
            session, settings, user, token_id=successor, family_id=token.family_id
        )
    if not was_revoked:
        # Unrevoked under the lock, yet the CAS matched nothing. Can't happen while
        # lock_user serialises token routes; kept as the benign answer if it ever does.
        raise _LostRotationRace(status_code=bad.status_code, detail=bad.detail)
    if rotated_at is None:
        # Revoked by logout, sign-out or a password change, not rotated: no chain was
        # continued with it, so it's no evidence of theft. Just refused.
        raise bad
    # A rotated token, presented again. Either a client died (or lost our reply to a
    # dropped connection, then stayed offline) before saving the new token, and now
    # replays the old one; or someone else holds a copy. Grace: within the window,
    # while the successor is still unused, hand out a fresh one in its place. The
    # successor is retired with NO successor of its own (CAS again, so two replays
    # can't both win): whoever presents it later gets a reuse verdict. That makes the
    # grace once per rotation. Were the retired successor grace-eligible too, a thief
    # and the device could keep retiring each other's token for as long as the window
    # (24 h), both signed in, never caught.
    grace = timedelta(seconds=settings.refresh_reuse_grace_seconds)
    if (
        not expired
        and replaced_by is not None
        and now - ensure_utc(rotated_at) <= grace
        and await _cas_rotate(session, replaced_by, None, now)
    ):
        record_event(session, user.id, "refresh_token_grace")
        return await _issue_tokens(
            session, settings, user, token_id=successor, family_id=token.family_id
        )
    # Theft, as far as we can tell: end this login's whole chain, whoever holds it.
    # (Also reached by an old rotated token replayed after sign-out-everywhere: the
    # UPDATE then changes nothing, but the event is still written. An operator reading
    # auth_events should expect that.)
    # Only this family: the user's other devices are not implicated, and revoking
    # them would sign the user out everywhere on every such event. Access tokens
    # already minted in the family live out their TTL (15 min, stateless JWTs).
    await session.execute(
        update(RefreshToken)
        .where(RefreshToken.family_id == token.family_id, RefreshToken.revoked.is_(False))
        .values(revoked=True)
    )
    record_event(session, user.id, "refresh_token_reuse")
    await session.commit()  # the revoke must outlive the 401
    raise bad


async def _cas_rotate(
    session: AsyncSession, token_id: uuid.UUID, successor: uuid.UUID | None, now: datetime
) -> bool:
    """Retire a live token as rotated into ``successor`` (None: retired by the grace,
    so presenting it later is reuse); False if it wasn't live."""
    result = cast(
        "CursorResult[Any]",
        await session.execute(
            update(RefreshToken)
            .where(RefreshToken.id == token_id, RefreshToken.revoked.is_(False))
            .values(revoked=True, rotated_at=now, replaced_by_id=successor)
        ),
    )
    return result.rowcount == 1


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


@router.post("/password", response_model=PasswordChangeOut)
async def change_password(
    body: PasswordChangeIn,
    request: Request,
    background: BackgroundTasks,
    user: Annotated[User, Depends(get_current_user)],
    session: Annotated[AsyncSession, Depends(get_session)],
    settings: Annotated[Settings, Depends(get_settings)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> PasswordChangeOut:
    """Change the caller's password and return a fresh pair for this client.

    With ``sign_out_other_devices`` (default): every session of the user is
    revoked at once, the caller's old one included (sign_out_everywhere), and the
    new pair keeps this client signed in. The old refresh token is dead from the
    moment this commits: a client that loses the response is signed out on its
    next refresh and signs in with the new password. Without it: only the
    password changes; every existing token, the caller's old one too, stays valid.

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
    # A TOTP pending token minted with the old password dies now, whatever the box
    # says; and the owner's remedy for "someone has my password" ends the code budget.
    user.password_changed_at = utcnow()
    limiter.code_reset(user.handle)
    record_event(session, user.id, "password_changed")
    if not body.sign_out_other_devices:
        kept = await _issue_tokens(session, settings, user)
        return PasswordChangeOut(**kept.model_dump(), other_devices_signed_out=False)
    cutoff_ms = await sign_out_everywhere(session, user)
    pair = await _issue_tokens(session, settings, user)  # commits; issued after the cutoff
    # Close the live sockets only AFTER this response is sent. The changing
    # device's own socket is among them; closed first, its client would refresh
    # with the old (now revoked) refresh token before it had the new pair, and
    # sign itself out. Other devices lose at most the time to send this response:
    # their access tokens are already refused on REST and on re-auth.
    background.add_task(revoke_sessions, user.id, cutoff_ms)
    return PasswordChangeOut(**pair.model_dump(), other_devices_signed_out=True)


@router.post("/logout", status_code=status.HTTP_204_NO_CONTENT)
async def logout(
    body: RefreshIn,
    session: Annotated[AsyncSession, Depends(get_session)],
) -> None:
    """End this device's login: every token of the presented token's family (idempotent).

    The family, not just the token: a refresh in flight when the user signs out has
    already rotated the presented token into a successor the client will discard,
    and revoking only the presented one would leave that successor live for its whole
    TTL. Any token of the family will do, rotated or not; one that already can't
    refresh (it was rotated) could end the family through /refresh anyway. The user's
    other devices are other families. Revoked here, not rotated: presenting one of
    these later is a plain 401, not a reuse. Always 204: an unknown token says nothing.
    """
    # Deliberately not rate-limited. The limiter's per-IP budget is shared with failed
    # logins, so a household behind one NAT address, throttled after a few mistyped
    # passwords, would get 429 on sign-out and keep its tokens live. There's nothing
    # to guess here (a token is 384 random bits), and each call is one indexed UPDATE.
    token = await session.scalar(
        select(RefreshToken).where(RefreshToken.token_hash == hash_token(body.refresh_token))
    )
    if token is None:
        return
    # The user lock, like every other revoke: a refresh rotating this family right now
    # holds it until its new token is committed. Without it, this UPDATE's snapshot
    # (READ COMMITTED) could predate that token and miss it, leaving it live (tested
    # on Postgres, test_token_races.py).
    await lock_user(session, token.user_id)
    result = cast(
        "CursorResult[Any]",
        await session.execute(
            update(RefreshToken)
            .where(RefreshToken.family_id == token.family_id, RefreshToken.revoked.is_(False))
            .values(revoked=True)
        ),
    )
    if result.rowcount:
        # A thief holding any family token can end the family here as via /refresh;
        # this keeps it on record, and explains a later reuse event on an old token.
        record_event(session, token.user_id, "logout")
    await session.commit()


@router.get("/me", response_model=MeOut)
async def me(
    user: Annotated[User, Depends(get_current_user)],
    session: Annotated[AsyncSession, Depends(get_session)],
) -> MeOut:
    """The current user, with their second-factor state (so the app shows Enable or
    Disable, and warns when recovery codes run low)."""
    enabled = (
        await session.scalar(
            select(Totp.id).where(Totp.user_id == user.id, Totp.activated_at.is_not(None))
        )
        is not None
    )
    left = None
    if enabled:
        left = int(
            await session.scalar(
                select(func.count())
                .select_from(RecoveryCode)
                .where(RecoveryCode.user_id == user.id, RecoveryCode.used_at.is_(None))
            )
            or 0
        )
    return MeOut(
        **UserOut.model_validate(user).model_dump(), totp_enabled=enabled, recovery_codes_left=left
    )
