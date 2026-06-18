"""Realtime WebSocket endpoint at ``/ws``.

Auth is the **first frame**: the client sends ``{"type":"auth","data":{"access_token":...}}``
within a short timeout, never as a query parameter (those leak into logs). The
socket is registered with the in-process hub so REST message sends fan out here.
Phase 1 carries server→client events only; client→server commands (typing, calls)
are read and ignored for now.
"""

from __future__ import annotations

import asyncio
import uuid
from typing import Annotated

import jwt
from fastapi import APIRouter, Depends, WebSocket, WebSocketDisconnect, status

from ..config import Settings, get_settings
from ..db import get_sessionmaker
from ..hub import Hub, get_hub
from ..models import User
from ..security import decode_access_token

router = APIRouter()

_AUTH_TIMEOUT_SECONDS = 10.0


async def _authenticate(ws: WebSocket, settings: Settings) -> User | None:
    """Resolve the user from the required first ``auth`` frame, or ``None``."""
    try:
        raw = await asyncio.wait_for(ws.receive_json(), timeout=_AUTH_TIMEOUT_SECONDS)
    except (TimeoutError, WebSocketDisconnect, ValueError):
        # timeout, client hung up, or non-JSON first frame
        return None

    if not isinstance(raw, dict) or raw.get("type") != "auth":
        return None
    data = raw.get("data") or {}
    token = data.get("access_token")
    if not isinstance(token, str) or not token:
        return None
    try:
        payload = decode_access_token(settings, token)
        if payload.get("type") != "access":
            return None
        user_id = uuid.UUID(str(payload["sub"]))
    except (jwt.PyJWTError, KeyError, ValueError):
        return None

    async with get_sessionmaker()() as session:
        user = await session.get(User, user_id)
    return user if user is not None and user.status == "active" else None


@router.websocket("/ws")
async def ws_endpoint(
    ws: WebSocket,
    settings: Annotated[Settings, Depends(get_settings)],
    hub: Annotated[Hub, Depends(get_hub)],
) -> None:
    await ws.accept()
    user = await _authenticate(ws, settings)
    if user is None:
        await ws.close(code=status.WS_1008_POLICY_VIOLATION)
        return

    await hub.register(user.id, ws)
    # Confirm the subscription so the client knows it will now receive fan-out
    # (and so senders racing a just-connected socket aren't silently missed).
    await ws.send_json({"type": "ready", "data": {"user_id": str(user.id)}})
    try:
        while True:
            # Phase 1: drain client frames (typing/call signals arrive later).
            await ws.receive_text()
    except WebSocketDisconnect:
        pass
    finally:
        await hub.unregister(user.id, ws)
