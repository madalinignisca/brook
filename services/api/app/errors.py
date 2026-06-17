"""Uniform error envelope for the whole API.

Every error response is rendered as Brook's own shape (docs/PROTOCOL.md §5):

    {"error": {"code": "<machine_code>", "message": "<human>", "details?": {}}}

This is deliberately *not* FastAPI's default ``{"detail": ...}``: the wire contract
that every native client parses is owned by us, not by the framework's defaults
(which can change across major FastAPI versions and would otherwise force a
lockstep refactor of all clients). Route code raises ``HTTPException`` with a
``{"code","message"}`` detail; these handlers reshape that — and framework-raised
errors (404, 405, validation, ...) — into the envelope above.
"""

from __future__ import annotations

import logging
from collections.abc import Mapping
from typing import Any, cast

from fastapi import FastAPI, Request
from fastapi.encoders import jsonable_encoder
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from starlette.exceptions import HTTPException

_log = logging.getLogger("brook.api")

# Stable machine codes for errors raised by the framework/routing rather than by
# our own code (which already supplies a code). Falls back to ``http_<status>``.
_STATUS_CODES: dict[int, str] = {
    400: "bad_request",
    401: "auth.unauthorized",
    403: "authz.forbidden",
    404: "not_found",
    405: "method_not_allowed",
    409: "conflict",
    413: "payload_too_large",
    415: "unsupported_media_type",
    429: "rate_limited",
    503: "service_unavailable",
}


def _envelope(
    status_code: int,
    code: str,
    message: str,
    details: Any | None = None,
    headers: Mapping[str, str] | None = None,
) -> JSONResponse:
    error: dict[str, Any] = {"code": code, "message": message}
    if details is not None:
        error["details"] = details
    return JSONResponse(status_code=status_code, content={"error": error}, headers=headers)


async def _http_exception(_request: Request, exc: Exception) -> JSONResponse:
    http_exc = cast("HTTPException", exc)
    detail = http_exc.detail
    headers = http_exc.headers
    # Our routes raise HTTPException(detail={"code","message"[,"details"]}).
    if isinstance(detail, dict) and "code" in detail and "message" in detail:
        return _envelope(
            http_exc.status_code,
            str(detail["code"]),
            str(detail["message"]),
            detail.get("details"),
            headers,
        )
    # Bare framework/routing errors carry a plain string detail.
    code = _STATUS_CODES.get(http_exc.status_code, f"http_{http_exc.status_code}")
    message = detail if isinstance(detail, str) else str(detail)
    return _envelope(http_exc.status_code, code, message, headers=headers)


async def _validation_exception(_request: Request, exc: Exception) -> JSONResponse:
    val_exc = cast("RequestValidationError", exc)
    return _envelope(
        422,
        "validation.error",
        "Request validation failed",
        details={"errors": jsonable_encoder(val_exc.errors())},
    )


async def _unhandled_exception(_request: Request, exc: Exception) -> JSONResponse:
    # Anything not raised as an HTTPException is a bug; log it with the traceback
    # but never leak internals to the client — and still honor the envelope so
    # clients never see FastAPI's default {"detail": ...} shape for a 500.
    _log.exception("Unhandled exception", exc_info=exc)
    return _envelope(500, "internal_error", "Internal server error")


def register_error_handlers(app: FastAPI) -> None:
    """Install handlers that render every error as the Brook ``{error:{...}}`` envelope."""
    app.add_exception_handler(HTTPException, _http_exception)
    app.add_exception_handler(RequestValidationError, _validation_exception)
    app.add_exception_handler(Exception, _unhandled_exception)
