"""Error-envelope contract tests (docs/PROTOCOL.md §5).

Every error — ours, the framework's, and *unanticipated* ones — must render as
``{"error": {"code", "message", "details?"}}`` and never FastAPI's default
``{"detail": ...}``. The 500 case is regression cover for the friends-review
finding that unhandled exceptions escaped the envelope.
"""

from __future__ import annotations

import httpx

from app.main import create_app


async def test_not_found_uses_envelope(client: httpx.AsyncClient) -> None:
    resp = await client.get("/api/v1/does-not-exist")
    assert resp.status_code == 404
    body = resp.json()
    assert "detail" not in body
    assert body["error"]["code"] == "not_found"


async def test_method_not_allowed_uses_envelope(client: httpx.AsyncClient) -> None:
    # /auth/login is POST-only; a GET is a framework-raised 405.
    resp = await client.get("/api/v1/auth/login")
    assert resp.status_code == 405
    assert resp.json()["error"]["code"] == "method_not_allowed"


async def test_unhandled_exception_uses_envelope() -> None:
    # A route that raises a non-HTTPException must still yield the envelope (500),
    # not FastAPI's default body and not a leaked traceback.
    app = create_app()

    @app.get("/api/v1/_boom")
    async def _boom() -> None:
        raise RuntimeError("kaboom")

    transport = httpx.ASGITransport(app=app, raise_app_exceptions=False)
    async with httpx.AsyncClient(transport=transport, base_url="http://test") as ac:
        resp = await ac.get("/api/v1/_boom")

    assert resp.status_code == 500
    body = resp.json()
    assert "detail" not in body
    assert body["error"]["code"] == "internal_error"
    assert "kaboom" not in resp.text  # never leak internals to the client
