"""Attachment sweep (attachments spec §7): a periodic task inside the api.

- pending files older than 1 h: rows, part files and any bytes go;
- committed files never attached within 24 h go;
- bytes on disk with no row (a crash between rename and commit, an account delete's
  cascade), and part files, go once they are older than the 1 h window, never earlier,
  so an upload in progress is never touched.
"""

from __future__ import annotations

import asyncio
import logging
import uuid
from datetime import timedelta

from fastapi.concurrency import run_in_threadpool
from sqlalchemy import delete, select

from . import files as storage
from .db import get_sessionmaker
from .models import File, utcnow

log = logging.getLogger(__name__)

PENDING_TTL = timedelta(hours=1)
UNATTACHED_TTL = timedelta(hours=24)
INTERVAL_S = 600


async def sweep_once() -> dict[str, int]:
    """One pass; returns what it removed (for logs and tests)."""
    now = utcnow()
    async with get_sessionmaker()() as session:
        stale_pending = list(
            (
                await session.scalars(
                    select(File.id).where(
                        File.status == "pending", File.created_at < now - PENDING_TTL
                    )
                )
            ).all()
        )
        unattached = list(
            (
                await session.scalars(
                    select(File.id).where(
                        File.status == "committed",
                        File.message_id.is_(None),
                        File.created_at < now - UNATTACHED_TTL,
                    )
                )
            ).all()
        )
        doomed = stale_pending + unattached
        if doomed:
            await session.execute(delete(File).where(File.id.in_(doomed)))
            await session.commit()
        known = set((await session.scalars(select(File.id))).all())

    for file_id in doomed:
        await run_in_threadpool(storage.remove, file_id)

    orphans = 0
    for path in await run_in_threadpool(storage.stale_entries, PENDING_TTL.total_seconds()):
        stem = path.name.split(".", 1)[0]
        try:
            file_id = uuid.UUID(stem)
        except ValueError:
            continue  # not ours; never touch what we didn't name
        is_part = path.name.endswith(storage.PART_SUFFIX)
        if is_part or file_id not in known:
            await run_in_threadpool(path.unlink, True)
            orphans += 1
    result = {"pending": len(stale_pending), "unattached": len(unattached), "orphans": orphans}
    if any(result.values()):
        log.info("attachment sweep: %s", result)
    return result


async def run_forever() -> None:
    """The lifespan's background loop. A failed pass is logged and retried next time."""
    while True:
        await asyncio.sleep(INTERVAL_S)
        try:
            await sweep_once()
        except Exception:
            log.exception("attachment sweep failed")
