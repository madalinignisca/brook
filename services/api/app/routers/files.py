"""Attachments over the api (attachments spec 2026-09-25, local filesystem).

Create (``POST /channels/{id}/files``), upload (``PUT /files/{id}/content``, raw
bytes streamed with a hard cap), download (``GET /files/{id}/content``, Range
supported) and delete. Attaching happens in ``POST /channels/{id}/messages``.
"""

from __future__ import annotations

import logging
import time
import uuid
from typing import Annotated, Any, cast

from fastapi import APIRouter, Depends, HTTPException, Request, Response, status
from fastapi.concurrency import run_in_threadpool
from fastapi.responses import FileResponse
from sqlalchemy import func, select, update
from sqlalchemy.engine import CursorResult
from sqlalchemy.ext.asyncio import AsyncSession

from .. import files as storage
from ..config import Settings, get_settings
from ..db import get_session
from ..deps import get_current_user
from ..filenames import original_name, safe_filename
from ..models import File, User, utcnow
from ..schemas import FileCreate, FileCreated, FileOut
from .channels import _membership, _require_member

log = logging.getLogger(__name__)

router = APIRouter(tags=["files"])

# Types a browser would execute or render as a document: served as plain bytes, so a
# file on our own origin can never run as chat.madalin.me (spec §5).
ACTIVE_TYPES = frozenset(
    {
        "text/html",
        "application/xhtml+xml",
        "image/svg+xml",
        "text/xml",
        "application/xml",
        "application/javascript",
        "text/javascript",
        "application/x-javascript",
        "text/css",
        "application/pdf",  # a PDF viewer runs script too; saved, not rendered here
    }
)

# Starting uploads: a small per-user bucket so a client can't flood pending rows.
_CREATE_BURST = 20.0
_CREATE_PER_S = 20.0 / 60.0
_create_buckets: dict[uuid.UUID, tuple[float, float]] = {}


def _create_allowed(user_id: uuid.UUID) -> int | None:
    now = time.monotonic()
    tokens, at = _create_buckets.get(user_id, (_CREATE_BURST, now))
    tokens = min(_CREATE_BURST, tokens + (now - at) * _CREATE_PER_S)
    if tokens < 1.0:
        _create_buckets[user_id] = (tokens, now)
        return max(1, int((1.0 - tokens) / _CREATE_PER_S) + 1)
    _create_buckets[user_id] = (tokens - 1.0, now)
    return None


def _error(code: int, err: str, message: str, details: object = None) -> HTTPException:
    detail: dict[str, object] = {"code": err, "message": message}
    if details is not None:
        detail["details"] = details
    return HTTPException(status_code=code, detail=detail)


def _not_found() -> HTTPException:
    return _error(status.HTTP_404_NOT_FOUND, "not_found", "No such file")


def _out(row: File) -> FileOut:
    return FileOut.model_validate(row)


@router.post(
    "/channels/{channel_id}/files",
    response_model=FileCreated,
    status_code=status.HTTP_201_CREATED,
)
async def create_file(
    channel_id: uuid.UUID,
    body: FileCreate,
    response: Response,
    user: Annotated[User, Depends(get_current_user)],
    session: Annotated[AsyncSession, Depends(get_session)],
    settings: Annotated[Settings, Depends(get_settings)],
) -> FileCreated:
    """Reserve a file: validate limits, record the names, return where to PUT it."""
    channel = await _require_member(session, channel_id, user)
    if channel.archived_at is not None:
        raise _error(status.HTTP_403_FORBIDDEN, "authz.forbidden", "This channel is archived")
    if body.client_id is not None:
        existing = await session.scalar(
            select(File).where(File.uploader_id == user.id, File.client_id == body.client_id)
        )
        if existing is not None:
            if existing.channel_id != channel_id:
                raise _error(
                    status.HTTP_409_CONFLICT, "conflict", "client_id used in another channel"
                )
            response.status_code = status.HTTP_200_OK
            return FileCreated(
                file=_out(existing), upload_url=f"/api/v1/files/{existing.id}/content"
            )
    if body.size > settings.files_max_bytes:
        raise _error(status.HTTP_413_CONTENT_TOO_LARGE, "file.too_large", "The file is too large")
    used = await session.scalar(
        select(func.coalesce(func.sum(File.size), 0)).where(File.uploader_id == user.id)
    )
    if int(used or 0) + body.size > settings.files_quota_bytes:
        raise _error(
            status.HTTP_413_CONTENT_TOO_LARGE, "file.quota_exceeded", "Your storage is full"
        )
    # The disk floor counts every pending upload's declared size as already used, so
    # parallel uploads can't jointly push the shared disk below it.
    pending = await session.scalar(
        select(func.coalesce(func.sum(File.size), 0)).where(File.status == "pending")
    )
    free = await run_in_threadpool(storage.free_bytes)
    if free - int(pending or 0) - body.size < settings.files_min_free_bytes:
        raise _error(507, "file.no_space", "The server is low on disk space")
    wait = _create_allowed(user.id)
    if wait is not None:
        raise HTTPException(
            status_code=status.HTTP_429_TOO_MANY_REQUESTS,
            detail={"code": "rate_limited", "message": "Too many uploads started"},
            headers={"Retry-After": str(wait)},
        )
    row = File(
        channel_id=channel_id,
        uploader_id=user.id,
        filename=safe_filename(body.filename),
        original_name=original_name(body.filename),
        size=body.size,
        content_type=body.content_type.strip().lower(),
        client_id=body.client_id,
    )
    session.add(row)
    await session.commit()
    await session.refresh(row)
    return FileCreated(file=_out(row), upload_url=f"/api/v1/files/{row.id}/content")


@router.put("/files/{file_id}/content", response_model=FileOut)
async def upload_content(
    file_id: uuid.UUID,
    request: Request,
    user: Annotated[User, Depends(get_current_user)],
    session: Annotated[AsyncSession, Depends(get_session)],
) -> FileOut:
    """Stream the raw body to this PUT's own part file with a hard cap, then commit it
    under the row lock (spec §3). Overlapping PUTs can't corrupt: each has its own part,
    and only the first to commit wins; the other gets 409 file.already_committed with
    the committed file, so an outbox retry can confirm its bytes won (sha256)."""
    row = await session.get(File, file_id)
    if row is None or row.uploader_id != user.id:
        raise _not_found()
    if row.status == "committed":
        raise _error(
            status.HTTP_409_CONFLICT,
            "file.already_committed",
            "Already uploaded",
            _out(row).model_dump(mode="json"),
        )
    declared = row.size
    await session.rollback()  # don't hold a transaction open while the body streams

    part = await run_in_threadpool(storage.PartWriter, file_id)
    try:
        async for chunk in request.stream():
            if part.size + len(chunk) > declared:
                raise _error(
                    status.HTTP_413_CONTENT_TOO_LARGE, "file.too_large", "More bytes than declared"
                )
            await run_in_threadpool(part.write, chunk)
        if part.size != declared:
            raise _error(422, "file.size_mismatch", "Fewer bytes than declared")
        await run_in_threadpool(part.finish)

        # Compare-and-set, not a read-then-write: only the PUT whose UPDATE flips
        # pending -> committed may rename its part into place. SELECT ... FOR UPDATE
        # is ignored by SQLite, so a read-then-write let two overlapping PUTs both
        # "win" and the later rename overwrite the committed bytes (tested).
        won = await session.execute(
            update(File)
            .where(File.id == file_id, File.status == "pending")
            .values(status="committed", sha256=part.sha256, committed_at=utcnow())
        )
        if cast("CursorResult[Any]", won).rowcount != 1:
            await session.rollback()
            current = await session.get(File, file_id)
            if current is None:
                raise _not_found()
            raise _error(
                status.HTTP_409_CONFLICT,
                "file.already_committed",
                "Already uploaded",
                _out(current).model_dump(mode="json"),
            )
        # Rename inside the winning transaction: a crash between the rename and the
        # commit leaves a pending row plus the file, which the sweep removes.
        await run_in_threadpool(part.commit_to, storage.final_path(file_id))
        await session.commit()
        committed = await session.get(File, file_id, populate_existing=True)
        assert committed is not None  # noqa: S101 - we just committed it
        return _out(committed)
    finally:
        await run_in_threadpool(part.discard)


def _download_type(content_type: str) -> str:
    base = content_type.split(";", 1)[0].strip().lower()
    if base in ACTIVE_TYPES or "script" in base or "html" in base or "xml" in base:
        return "application/octet-stream"
    return base or "application/octet-stream"


@router.get("/files/{file_id}/content")
async def download_content(
    file_id: uuid.UUID,
    user: Annotated[User, Depends(get_current_user)],
    session: Annotated[AsyncSession, Depends(get_session)],
) -> FileResponse:
    """Stream a committed file to a member of its channel. Range is supported (resume),
    and the headers make sure it is saved, never rendered or run (spec §5). A pending
    file is 404: it doesn't exist yet for anyone."""
    row = await session.get(File, file_id)
    if row is None or row.status != "committed":
        raise _not_found()
    if await _membership(session, row.channel_id, user.id) is None:
        raise _not_found()  # 404, not 403: don't reveal the file exists
    path = storage.final_path(file_id)
    if not path.is_file():
        log.error("committed file missing on disk: %s", file_id)
        raise _not_found()
    return FileResponse(
        path,
        media_type=_download_type(row.content_type),
        filename=row.filename,
        content_disposition_type="attachment",
        headers={
            "X-Content-Type-Options": "nosniff",
            "Content-Security-Policy": "sandbox; default-src 'none'",
            "Cross-Origin-Resource-Policy": "same-origin",
            "Cache-Control": "private, no-store",
            # The content hash, quoted, as the ETag: a resumed download sends
            # If-Range: "<sha256>" and Starlette compares it with this exactly (its
            # default ETag is mtime+size, which a client can't know in advance).
            "ETag": f'"{row.sha256}"',
        },
    )


@router.delete("/files/{file_id}", status_code=status.HTTP_204_NO_CONTENT)
async def delete_file(
    file_id: uuid.UUID,
    user: Annotated[User, Depends(get_current_user)],
    session: Annotated[AsyncSession, Depends(get_session)],
) -> None:
    """Remove a file (the uploader, or an owner of its channel). An attached file
    leaves its message showing it was removed."""
    row = await session.get(File, file_id)
    if row is None:
        raise _not_found()
    membership = await _membership(session, row.channel_id, user.id)
    if membership is None:
        raise _not_found()
    if row.uploader_id != user.id and membership.role != "owner":
        raise _error(status.HTTP_403_FORBIDDEN, "authz.forbidden", "Not your file")
    await session.delete(row)
    await session.commit()
    await run_in_threadpool(storage.remove, file_id)
