"""FastAPI application factory."""

from __future__ import annotations

from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from pathlib import Path

from fastapi import FastAPI
from fastapi.responses import FileResponse
from sqlalchemy import select

from . import __version__, calls
from .config import get_settings
from .db import get_sessionmaker, init_models
from .errors import register_error_handlers
from .models import Totp
from .routers import auth, channels, health, totp, users, ws
from .secretbox import DecryptError, Purpose, get_secret_box


@asynccontextmanager
async def lifespan(_app: FastAPI) -> AsyncIterator[None]:
    """Startup/shutdown. Refuse to boot with an insecure JWT key or keyring; init schema."""
    get_settings().assert_secure()
    # Build the keyring now: after this, DecryptError is the only runtime failure type.
    get_secret_box()
    await init_models()
    await secret_canary()
    yield


async def secret_canary() -> None:
    """Encryption spec §5.8: decrypt one stored TOTP secret at boot and refuse to
    serve if it fails. A key dropped too early then stops the deploy, instead of
    locking users out one at a time over days. No enrolments yet: nothing to check.
    (Pending enrolments count too: they hold ciphertext under the same keyring.)"""
    async with get_sessionmaker()() as session:
        row = await session.scalar(select(Totp).order_by(Totp.created_at).limit(1))
    if row is None:
        return
    try:
        get_secret_box().decrypt(row.secret, purpose=Purpose.TOTP_SECRET, row_pk=row.id)
    except DecryptError as exc:
        raise RuntimeError(
            f"secret canary failed: a stored TOTP secret does not decrypt ({exc.reason}). "
            "Was a key removed from BROOK_SECRET_KEYS before rewrapping?"
        ) from None


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
    app.include_router(totp.router, prefix="/api/v1")
    app.include_router(channels.router, prefix="/api/v1")
    app.include_router(users.router, prefix="/api/v1")
    app.include_router(ws.router)  # /ws at the root, not under /api/v1
    # Importing app.calls registers the call.* WebSocket commands; holding the
    # manager on app.state makes that dependency explicit, so no tool (or person)
    # removes the import as unused. Guarded by tests/test_calls_unit.py.
    app.state.calls = calls.manager
    if get_settings().dev_harness:
        harness = Path(__file__).parent / "static" / "call_harness.html"

        @app.get("/dev/call", include_in_schema=False)
        async def dev_call_harness() -> FileResponse:
            return FileResponse(harness, media_type="text/html")

    return app


app = create_app()
