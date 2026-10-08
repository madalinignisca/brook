# SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
# SPDX-License-Identifier: AGPL-3.0-or-later

"""Liveness/readiness endpoint."""

from __future__ import annotations

from fastapi import APIRouter

from .. import __version__

router = APIRouter(tags=["health"])


@router.get("/health")
async def health() -> dict[str, str]:
    """Return service liveness and version."""
    return {"status": "ok", "version": __version__}
