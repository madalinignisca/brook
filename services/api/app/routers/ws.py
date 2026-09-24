"""Realtime WebSocket endpoint at ``/ws`` (PROTOCOL.md §2).

Auth is the **first frame**: ``{"type":"auth","data":{"access_token":...}}`` within
``AUTH_TIMEOUT_S``, never as a query parameter (those leak into logs). The server
answers ``{"type":"ready"}`` and registers the socket with the in-process hub so
REST message sends fan out here. Any auth failure closes with 1008; the close
reason says which (``auth_failed``, ``auth_timeout``, ``token_expired``).

After ``ready`` the socket also carries **commands** (call signaling, §3): each
client frame carries an ``id`` and gets exactly one reply carrying ``re`` (its
success frame or an ``error``). Handlers register with :func:`handles`.

The socket closes itself when its access token expires; sending ``auth`` again on
the open socket with a fresh token (same user) extends it instead. Without that
a socket authorized once would outlive an expired or revoked session.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import time
import uuid
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any

import jwt
from fastapi import APIRouter, WebSocket, WebSocketDisconnect, status

from ..config import Settings, get_settings
from ..db import get_sessionmaker
from ..hub import get_hub
from ..models import User
from ..security import decode_access_token

log = logging.getLogger(__name__)
router = APIRouter()

AUTH_TIMEOUT_S = 5.0  # SECURITY.md §7 "WS first-frame auth timeout"
MAX_FRAME_BYTES = 64 * 1024  # SDPs are ~5-20 KB; anything near this is abuse
CLOSE_POLICY = status.WS_1008_POLICY_VIOLATION
CLOSE_TOO_LARGE = status.WS_1009_MESSAGE_TOO_BIG


def envelope(type_: str, data: dict[str, Any], re: str | None = None) -> dict[str, Any]:
    """A server frame: ``{type, id, ts, data}`` (+ ``re`` for a direct reply)."""
    frame: dict[str, Any] = {
        "type": type_,
        "id": str(uuid.uuid4()),
        "ts": datetime.now(UTC).isoformat(),
        "data": data,
    }
    if re is not None:
        frame["re"] = re
    return frame


def error_frame(re: str | None, code: str, message: str) -> dict[str, Any]:
    """An ``error`` reply (codes: PROTOCOL.md §3.2)."""
    return envelope("error", {"code": code, "message": message}, re=re)


def _frame_id(frame: dict[str, Any]) -> str | None:
    fid = frame.get("id")
    return fid if isinstance(fid, str) else None


@dataclass(eq=False)
class Connection:
    """One authenticated socket. A user may hold several (devices, reconnects)."""

    ws: WebSocket
    user_id: uuid.UUID
    conn_id: str = field(default_factory=lambda: str(uuid.uuid4()))
    closed: bool = False
    expiry: asyncio.Task[None] | None = None

    async def send(self, frame: dict[str, Any]) -> None:
        """Best effort: a send to a client that has gone never raises.

        Clients may close right after a command (an app quitting after
        call.leave). If a send there raised, the handler stopped before its real
        work and state went stale: a "left" participant lingered as a ghost for
        the whole reconnect grace. Mark the socket closed instead; the read loop
        and the disconnect hooks do the cleanup.
        """
        if self.closed:
            return  # never write after close; Starlette raises if we do
        try:
            await get_hub().send(self.ws, frame)
        except Exception:  # noqa: BLE001 - the peer is gone; nothing to tell it
            self.closed = True

    async def close(self, code: int, reason: str) -> None:
        """Close once. The read loop checks ``closed`` and stops handling frames:
        a client can still have frames in flight when we close (e.g. at token
        expiry), and handling them would reply on a closed socket."""
        if self.closed:
            return
        self.closed = True
        with contextlib.suppress(Exception):
            await self.ws.close(code=code, reason=reason)


Handler = Callable[[Connection, dict[str, Any]], Awaitable[None]]
Hook = Callable[[Connection], Awaitable[None]]
_handlers: dict[str, Handler] = {}
connect_hooks: list[Hook] = []  # after `ready`, e.g. the channel.call snapshot
disconnect_hooks: list[Hook] = []  # e.g. start a call participant's resume grace


def handles(type_: str) -> Callable[[Handler], Handler]:
    """Register the handler for a client command type (e.g. ``call.join``)."""

    def deco(fn: Handler) -> Handler:
        _handlers[type_] = fn
        return fn

    return deco


@handles("ping")
async def _ping(conn: Connection, frame: dict[str, Any]) -> None:
    await conn.send(envelope("pong", {}, re=_frame_id(frame)))


async def _user_from_token(settings: Settings, token: object) -> tuple[User, int] | None:
    """``(active user, exp)`` for a valid access token, else ``None``."""
    if not isinstance(token, str) or not token:
        return None
    try:
        payload = decode_access_token(settings, token)
        if payload.get("type") != "access":
            return None
        user_id = uuid.UUID(str(payload["sub"]))
        exp = int(payload["exp"])
    except (jwt.PyJWTError, KeyError, ValueError, TypeError):
        return None
    async with get_sessionmaker()() as session:
        user = await session.get(User, user_id)
    if user is None or user.status != "active":
        return None
    return user, exp


def _auth_token(frame: object) -> object:
    if not isinstance(frame, dict) or frame.get("type") != "auth":
        return None
    data = frame.get("data")
    return data.get("access_token") if isinstance(data, dict) else None


async def _expire(conn: Connection, exp: int) -> None:
    await asyncio.sleep(max(0.0, exp - time.time()))
    await conn.close(CLOSE_POLICY, "token_expired")


async def _reauth(conn: Connection, frame: dict[str, Any], settings: Settings) -> None:
    """``auth`` on the open socket: swap in a fresh token for the same user."""
    authed = await _user_from_token(settings, _auth_token(frame))
    if authed is None or authed[0].id != conn.user_id:
        await conn.close(CLOSE_POLICY, "auth_failed")
        return
    if conn.expiry is not None:
        conn.expiry.cancel()
    conn.expiry = asyncio.create_task(_expire(conn, authed[1]))
    await conn.send(envelope("ready", {"user_id": str(conn.user_id)}, re=_frame_id(frame)))


@router.websocket("/ws")
async def ws_endpoint(ws: WebSocket) -> None:
    await ws.accept()
    settings = get_settings()
    try:
        raw = await asyncio.wait_for(ws.receive_text(), timeout=AUTH_TIMEOUT_S)
    except TimeoutError:
        await ws.close(code=CLOSE_POLICY, reason="auth_timeout")
        return
    except WebSocketDisconnect:
        return
    except KeyError:
        # A binary first frame: Starlette's receive_text() raises KeyError("text").
        await ws.close(code=status.WS_1003_UNSUPPORTED_DATA, reason="auth_failed")
        return
    try:
        first = json.loads(raw)
    except ValueError:
        first = None
    authed = await _user_from_token(settings, _auth_token(first))
    if authed is None:
        await ws.close(code=CLOSE_POLICY, reason="auth_failed")
        return
    user, exp = authed
    hub = get_hub()
    await hub.register(user.id, ws)
    conn = Connection(ws=ws, user_id=user.id)
    conn.expiry = asyncio.create_task(_expire(conn, exp))
    try:
        # Confirm the subscription so the client knows it will now receive fan-out
        # (and so senders racing a just-connected socket aren't silently missed).
        ready_re = _frame_id(first) if isinstance(first, dict) else None
        await conn.send(envelope("ready", {"user_id": str(user.id)}, re=ready_re))
        for hook in connect_hooks:
            try:
                await hook(conn)
            except Exception:
                log.exception("connect hook failed")
        # `closed` is checked before every read, not just after: a handler (e.g. a
        # failed re-auth) may have closed the socket, and reading a closed Starlette
        # WebSocket raises instead of returning.
        while not conn.closed:
            raw = await ws.receive_text()
            if conn.closed:
                return
            if len(raw.encode()) > MAX_FRAME_BYTES:
                await conn.close(CLOSE_TOO_LARGE, "frame_too_large")
                return
            try:
                frame = json.loads(raw)
                if not isinstance(frame, dict) or not isinstance(frame.get("type"), str):
                    raise ValueError("not an envelope")
            except ValueError:
                await conn.send(error_frame(None, "invalid", "frame is not a JSON envelope"))
                continue
            if frame["type"] == "auth":
                await _reauth(conn, frame, settings)
                continue
            handler = _handlers.get(frame["type"])
            if handler is None:
                await conn.send(
                    error_frame(_frame_id(frame), "invalid", f"unknown type {frame['type']!r}")
                )
                continue
            try:
                await handler(conn, frame)
            except Exception:  # a handler bug must not kill the socket
                log.exception("handler %s failed", frame["type"])
                await conn.send(error_frame(_frame_id(frame), "invalid", "internal error"))
    except WebSocketDisconnect:
        pass
    finally:
        if conn.expiry is not None:
            conn.expiry.cancel()
        await hub.unregister(user.id, ws)
        for hook in disconnect_hooks:
            try:
                await hook(conn)
            except Exception:
                log.exception("disconnect hook failed")
