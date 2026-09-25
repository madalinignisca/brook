"""TOTP (optional 2FA) for local accounts: spec 2026-09-25-totp-server-design.md.

The login step (``POST /auth/totp``) and the management routes (enroll, activate,
disable, regenerate recovery codes). Every route that accepts a code goes through
:func:`verify_second_factor`, so the replay guard, the code budget and the
decrypt-failure handling can't differ between them.
"""

from __future__ import annotations

import logging
import time
import uuid
from datetime import timedelta
from typing import Annotated

import jwt
from fastapi import APIRouter, BackgroundTasks, Depends, HTTPException, Request, status
from sqlalchemy import delete, func, select
from sqlalchemy.ext.asyncio import AsyncSession

from .. import totp as totp_core
from ..config import Settings, get_settings
from ..db import get_session
from ..deps import get_current_user
from ..events import record_event
from ..models import RecoveryCode, Totp, User, ensure_utc, utcnow
from ..ratelimit import AuthLimiter, client_ip, enforce, get_limiter
from ..schemas import (
    CodeIn,
    PasswordIn,
    RecoveryCodesOut,
    SecondFactorIn,
    TotpActivateOut,
    TotpEnrollOut,
    TotpLoginOut,
    TotpStepIn,
)
from ..secretbox import DecryptError, Purpose, get_secret_box
from ..security import decode_totp_pending_token, issued_at_ms, verify_password
from .auth import _issue_tokens, lock_user, sign_out_everywhere
from .ws import revoke_sessions

log = logging.getLogger(__name__)

router = APIRouter(prefix="/auth/totp", tags=["totp"])

ENROLL_TTL_S = 600

# Wall clock for TOTP steps; a module attribute so tests can step through 30 s
# windows without sleeping.
_now = time.time

# Used pending-token ids until their expiry (unix seconds). Single worker, like the
# hub and the socket registry: across a restart one password entry could complete
# two logins within 300 s, but only with two different valid codes (the DB-side
# last_used_step refuses a reused one), so a duplicate session, never a bypass.
_used_jti: dict[str, float] = {}


def _claim_jti(jti: str, exp: float) -> bool:
    """Mark ``jti`` used; False if it already was. No await between the check and
    the insert, and callers hold the user-row lock, so two requests can't both win."""
    now = time.time()
    for old in [j for j, e in _used_jti.items() if e < now]:
        del _used_jti[old]
    if jti in _used_jti:
        return False
    _used_jti[jti] = exp
    return True


def _forbidden(code: str, message: str) -> HTTPException:
    return HTTPException(
        status_code=status.HTTP_403_FORBIDDEN, detail={"code": code, "message": message}
    )


def _too_many(wait: int) -> HTTPException:
    return HTTPException(
        status_code=status.HTTP_429_TOO_MANY_REQUESTS,
        detail={"code": "auth.rate_limited", "message": "Too many attempts"},
        headers={"Retry-After": str(wait)},
    )


INVALID_CODE = ("auth.invalid_code", "The code is not valid")


async def active_totp(session: AsyncSession, user_id: uuid.UUID) -> Totp | None:
    row: Totp | None = await session.scalar(
        select(Totp).where(Totp.user_id == user_id, Totp.activated_at.is_not(None))
    )
    return row


async def recovery_codes_left(session: AsyncSession, user_id: uuid.UUID) -> int:
    n = await session.scalar(
        select(func.count())
        .select_from(RecoveryCode)
        .where(RecoveryCode.user_id == user_id, RecoveryCode.used_at.is_(None))
    )
    return int(n or 0)


async def replace_recovery_codes(session: AsyncSession, user_id: uuid.UUID) -> list[str]:
    """Delete every code of the user (used or not) and store 10 fresh ones."""
    await session.execute(delete(RecoveryCode).where(RecoveryCode.user_id == user_id))
    fresh = totp_core.new_recovery_codes()
    for c in fresh:
        session.add(RecoveryCode(user_id=user_id, lookup=c.lookup, code_hash=c.hash))
    return [c.display for c in fresh]


async def remove_totp(session: AsyncSession, user_id: uuid.UUID) -> None:
    await session.execute(delete(Totp).where(Totp.user_id == user_id))
    await session.execute(delete(RecoveryCode).where(RecoveryCode.user_id == user_id))


def _stored_key_id(stored: str) -> str:
    """The key id a stored value claims (``v1.<id>.…``), for logs only: it tells the
    operator which key was dropped. Never the value itself."""
    parts = stored.split(".", 2)
    return parts[1] if len(parts) > 2 and parts[1].isdigit() else "?"


def _decrypt_secret(row: Totp) -> str:
    return get_secret_box().decrypt(row.secret, purpose=Purpose.TOTP_SECRET, row_pk=row.id)


async def verify_second_factor(
    session: AsyncSession,
    limiter: AuthLimiter,
    ip: str,
    user: User,
    row: Totp,
    *,
    code: str | None,
    recovery_code: str | None,
) -> int | None:
    """Check a TOTP code or a recovery code for ``user`` (caller holds the user lock).

    On success returns ``None`` for a code, or the recovery codes left, and records the
    use (``last_used_step`` / ``used_at``). On failure raises 403 ``auth.invalid_code``
    or 429. A decrypt failure of the secret is answered exactly like a wrong code (fail
    closed, never "not enrolled", never a 500) but is an operator fault: logged at
    ERROR, counted by secretbox.decrypt_failures, and NOT a limiter failure, with its
    own budget so it can't amplify (spec §5).
    """
    wait = limiter.code_check(user.handle, ip)
    if wait is not None:
        raise _too_many(wait)

    if code is not None:
        wait = limiter.decrypt_check(user.handle)
        if wait is not None:
            raise _too_many(wait)
        try:
            secret = _decrypt_secret(row)
        except DecryptError as exc:
            limiter.decrypt_failure(user.handle)
            log.error(
                "totp secret decrypt failed: user=%s key_id=%s reason=%s",
                user.id,
                _stored_key_id(row.secret),
                exc.reason,
            )
            raise _forbidden(*INVALID_CODE) from None
        step = totp_core.match_step(secret, code, _now(), row.last_used_step)
        if step is None:
            _count_wrong(session, limiter, ip, user)
            raise _forbidden(*INVALID_CODE)
        row.last_used_step = step
        box = get_secret_box()
        if box.needs_rewrap(row.secret):  # key rotation: move to the primary key
            row.secret = box.encrypt(secret, purpose=Purpose.TOTP_SECRET, row_pk=row.id)
        return None

    parsed = totp_core.parse_recovery_code(recovery_code or "")
    stored: RecoveryCode | None = None
    if parsed is not None:
        stored = await session.scalar(
            select(RecoveryCode).where(
                RecoveryCode.user_id == user.id,
                RecoveryCode.lookup == parsed[0],
                RecoveryCode.used_at.is_(None),
            )
        )
    ok = totp_core.verify_recovery_secret(
        stored.code_hash if stored is not None else None, parsed[1] if parsed else "x" * 16
    )
    if not ok or stored is None:
        _count_wrong(session, limiter, ip, user)
        raise _forbidden(*INVALID_CODE)
    stored.used_at = utcnow()
    record_event(session, user.id, "recovery_code_used")
    await session.flush()
    return await recovery_codes_left(session, user.id)


def _count_wrong(session: AsyncSession, limiter: AuthLimiter, ip: str, user: User) -> None:
    limiter.failure(ip, user.handle)
    if limiter.code_failure(user.handle):
        record_event(session, user.id, "totp_guessing")


async def _commit_then_raise(session: AsyncSession, exc: HTTPException) -> None:
    """Keep events recorded on a failed attempt (e.g. totp_guessing), then raise."""
    await session.commit()
    raise exc


# ---------------------------------------------------------------- login step


def _expired() -> HTTPException:
    return _forbidden("auth.totp_expired", "Sign in again")


@router.post("", response_model=TotpLoginOut)
async def totp_login(
    body: TotpStepIn,
    request: Request,
    session: Annotated[AsyncSession, Depends(get_session)],
    settings: Annotated[Settings, Depends(get_settings)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> TotpLoginOut:
    """The second half of a TOTP login (spec §2.1). Never 401: core treats 401 as
    "refresh your access token", and this caller has none."""
    ip = client_ip(request.client.host if request.client else None)
    enforce(limiter, ip)
    try:
        payload = decode_totp_pending_token(settings, body.totp_token)
        user_id = uuid.UUID(str(payload["sub"]))
        issued_ms = issued_at_ms(payload)
        jti, exp = str(payload["jti"]), float(payload["exp"])
    except (jwt.PyJWTError, KeyError, ValueError, TypeError):
        limiter.failure(ip)  # a forged or foreign token is a probe
        raise _expired() from None

    user = await lock_user(session, user_id)  # serialises the jti claim and the replay guard
    if (
        user is None
        or user.status != "active"
        or user.session_revoked(issued_ms)
        or user.password_changed_since(issued_ms)
        or jti in _used_jti
    ):
        raise _expired()
    row = await active_totp(session, user.id)
    # Disabled, or reset and re-enrolled, since the password was proven: start over.
    if row is None or int(ensure_utc(row.activated_at).timestamp() * 1000) > issued_ms:  # type: ignore[arg-type]
        raise _expired()

    try:
        left = await verify_second_factor(
            session, limiter, ip, user, row, code=body.code, recovery_code=body.recovery_code
        )
    except HTTPException as exc:
        if exc.status_code == status.HTTP_403_FORBIDDEN:
            await _commit_then_raise(session, exc)
        raise
    if not _claim_jti(jti, exp):
        raise _expired()
    limiter.success(ip, user.handle)  # only now: a completed login earns trust
    limiter.code_reset(user.handle)
    pair = await _issue_tokens(session, settings, user)  # commits
    return TotpLoginOut(**pair.model_dump(), recovery_codes_left=left)


# ---------------------------------------------------------------- management


async def _reauth_password(
    session: AsyncSession, limiter: AuthLimiter, request: Request, user: User, password: str
) -> tuple[User, str]:
    """Rate-limited password re-check under the user lock; returns the locked user."""
    ip = client_ip(request.client.host if request.client else None)
    enforce(limiter, ip, user.handle)
    locked = await lock_user(session, user.id)
    if locked is None or locked.status != "active":
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail={"code": "auth.invalid_token", "message": "Invalid or expired token"},
        )
    if locked.password_hash is None or not verify_password(locked.password_hash, password):
        limiter.failure(ip, locked.handle)
        raise _forbidden("auth.invalid_credentials", "Password is wrong")
    # No handle: a password re-check resets the IP's streak but is not a completed
    # login, so it must not make this IP trusted (exempt from the code budget).
    limiter.success(ip)
    return locked, ip


@router.post("/enroll", response_model=TotpEnrollOut)
async def enroll(
    body: PasswordIn,
    request: Request,
    user: Annotated[User, Depends(get_current_user)],
    session: Annotated[AsyncSession, Depends(get_session)],
    settings: Annotated[Settings, Depends(get_settings)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> TotpEnrollOut:
    """Start enrolment: a fresh secret, returned once as an otpauth URI (never by a
    GET). Refused while TOTP is active; replaces an unfinished enrolment."""
    user, _ip = await _reauth_password(session, limiter, request, user, body.password)
    existing = await session.scalar(select(Totp).where(Totp.user_id == user.id))
    if existing is not None and existing.activated_at is not None:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail={"code": "conflict", "message": "TOTP is already active"},
        )
    if existing is not None:
        await session.delete(existing)
        await session.flush()
    secret = totp_core.new_secret()
    row_id = uuid.uuid4()  # the AAD row_pk, so it must exist before encrypting
    session.add(
        Totp(
            id=row_id,
            user_id=user.id,
            secret=get_secret_box().encrypt(secret, purpose=Purpose.TOTP_SECRET, row_pk=row_id),
            pending_expires_at=utcnow() + timedelta(seconds=ENROLL_TTL_S),
        )
    )
    record_event(session, user.id, "totp_enrolled")
    await session.commit()
    uri = totp_core.otpauth_uri(secret, issuer=settings.totp_issuer, account=user.handle)
    return TotpEnrollOut(otpauth_uri=uri, expires_in=ENROLL_TTL_S)


@router.post("/activate", response_model=TotpActivateOut)
async def activate(
    body: CodeIn,
    request: Request,
    background: BackgroundTasks,
    user: Annotated[User, Depends(get_current_user)],
    session: Annotated[AsyncSession, Depends(get_session)],
    settings: Annotated[Settings, Depends(get_settings)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> TotpActivateOut:
    """Prove the authenticator works; TOTP becomes active, recovery codes are issued
    (shown once), and every other session is signed out: a token stolen before
    enrolment must not keep the account open without the new factor."""
    ip = client_ip(request.client.host if request.client else None)
    enforce(limiter, ip, user.handle)
    locked = await lock_user(session, user.id)
    if locked is None:
        raise _expired()
    user = locked
    row = await session.scalar(select(Totp).where(Totp.user_id == user.id))
    if row is None or row.activated_at is not None:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail={"code": "conflict", "message": "No enrolment in progress"},
        )
    if row.pending_expires_at is None or ensure_utc(row.pending_expires_at) <= utcnow():
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail={"code": "auth.totp_enrollment_expired", "message": "Scan a new code"},
        )
    try:
        await verify_second_factor(
            session, limiter, ip, user, row, code=body.code, recovery_code=None
        )
    except HTTPException as exc:
        if exc.status_code == status.HTTP_403_FORBIDDEN:
            await _commit_then_raise(session, exc)
        raise
    row.activated_at = utcnow()
    row.pending_expires_at = None
    codes = await replace_recovery_codes(session, user.id)
    record_event(session, user.id, "totp_activated")
    limiter.code_reset(user.handle)
    cutoff_ms = await sign_out_everywhere(session, user)
    pair = await _issue_tokens(session, settings, user)  # commits; issued after the cutoff
    background.add_task(revoke_sessions, user.id, cutoff_ms)  # after the response (#45)
    return TotpActivateOut(**pair.model_dump(), recovery_codes=codes)


async def _manage(
    body: SecondFactorIn,
    request: Request,
    user: User,
    session: AsyncSession,
    limiter: AuthLimiter,
) -> tuple[User, Totp]:
    user, ip = await _reauth_password(session, limiter, request, user, body.password)
    row = await active_totp(session, user.id)
    if row is None:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail={"code": "conflict", "message": "TOTP is not active"},
        )
    code, recovery = body.code, body.recovery_code
    # Spec §2.3: here `code` may also be a recovery code (one field in the app's form).
    # Six digits is a TOTP code; anything else is tried as a recovery code.
    if code is not None and not _looks_like_totp(code):
        code, recovery = None, code
    try:
        await verify_second_factor(
            session, limiter, ip, user, row, code=code, recovery_code=recovery
        )
    except HTTPException as exc:
        if exc.status_code == status.HTTP_403_FORBIDDEN:
            await _commit_then_raise(session, exc)
        raise
    limiter.code_reset(user.handle)
    return user, row


def _looks_like_totp(text: str) -> bool:
    digits = text.strip().replace(" ", "")
    return len(digits) == totp_core.DIGITS and digits.isascii() and digits.isdigit()


@router.post("/disable", status_code=status.HTTP_204_NO_CONTENT)
async def disable(
    body: SecondFactorIn,
    request: Request,
    user: Annotated[User, Depends(get_current_user)],
    session: Annotated[AsyncSession, Depends(get_session)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> None:
    """Turn TOTP off: password plus a current code or a recovery code."""
    user, _row = await _manage(body, request, user, session, limiter)
    await remove_totp(session, user.id)
    record_event(session, user.id, "totp_disabled")
    await session.commit()


@router.post("/recovery-codes", response_model=RecoveryCodesOut)
async def regenerate_recovery_codes(
    body: SecondFactorIn,
    request: Request,
    user: Annotated[User, Depends(get_current_user)],
    session: Annotated[AsyncSession, Depends(get_session)],
    limiter: Annotated[AuthLimiter, Depends(get_limiter)],
) -> RecoveryCodesOut:
    """10 fresh codes; every old one (used or not) stops working."""
    user, _row = await _manage(body, request, user, session, limiter)
    codes = await replace_recovery_codes(session, user.id)
    record_event(session, user.id, "recovery_codes_regenerated")
    await session.commit()
    return RecoveryCodesOut(recovery_codes=codes)
