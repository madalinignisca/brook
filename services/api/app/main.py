"""FastAPI application factory."""

from __future__ import annotations

from collections.abc import AsyncIterator
from contextlib import asynccontextmanager

from fastapi import FastAPI

from . import __version__
from .config import get_settings
from .db import init_models
from .errors import register_error_handlers
from .routers import auth, channels, health, ws


@asynccontextmanager
async def lifespan(_app: FastAPI) -> AsyncIterator[None]:
    """Startup/shutdown. Refuse to boot with an insecure JWT key; init schema (Phase 0)."""
    get_settings().assert_secure()
    await init_models()
    yield


def create_app() -> FastAPI:
    """Build and configure the FastAPI app."""
    # redirect_slashes=False: a JSON API never redirects. FastAPI's default 307 on a
    # trailing slash re-sends the body (passwords included) to the Location URL, and
    # behind TLS-terminating Caddy that URL is http:// -- an https->http downgrade.
    # A wrong path is a 404. Guarded by tests/test_no_redirects.py.
    app = FastAPI(title="Brook API", version=__version__, lifespan=lifespan, redirect_slashes=False)
    register_error_handlers(app)
    app.include_router(health.router)
    app.include_router(auth.router, prefix="/api/v1")
    app.include_router(channels.router, prefix="/api/v1")
    app.include_router(ws.router)  # /ws at the root, not under /api/v1
    return app


app = create_app()
