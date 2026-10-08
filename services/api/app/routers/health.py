# SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
# SPDX-License-Identifier: AGPL-3.0-or-later

"""Liveness/readiness endpoint."""

from __future__ import annotations

from fastapi import APIRouter

from .. import __version__
from ..config import get_settings

router = APIRouter(tags=["health"])


@router.get("/health")
async def health() -> dict[str, str]:
    """Return service liveness, version and where this server's source is.

    Public on purpose and reads no token: AGPL-3.0 section 13 says every user of a modified
    server must be able to find its source, including before they sign in (spec
    2026-10-08-agpl-source-url section 4.2). A new field here, not a new route, so there is
    no new auth rule to get wrong.
    """
    return {"status": "ok", "version": __version__, "source_url": get_settings().source_url}
