"""In-process realtime hub.

Tracks live WebSocket connections per user and fans out events to a set of users
(a channel's members). Single-node only — scale-out (sticky sessions + Redis/NATS
pub-sub) is deferred per the roadmap. The hub is process-global; one per api node.
"""

from __future__ import annotations

import asyncio
import uuid
from collections import defaultdict
from typing import Any

from starlette.websockets import WebSocket


class Hub:
    """A registry of `user_id -> set[WebSocket]` with event fan-out."""

    def __init__(self) -> None:
        self._conns: dict[uuid.UUID, set[WebSocket]] = defaultdict(set)
        self._lock = asyncio.Lock()
        # One writer per socket. Fan-out (chat events) and direct replies (call
        # signaling) can target the same socket from different tasks; unserialized
        # concurrent sends on one ASGI WebSocket are not safe.
        self._send_locks: dict[WebSocket, asyncio.Lock] = {}

    async def register(self, user_id: uuid.UUID, ws: WebSocket) -> None:
        async with self._lock:
            self._conns[user_id].add(ws)
            self._send_locks.setdefault(ws, asyncio.Lock())

    async def send(self, ws: WebSocket, event: dict[str, Any]) -> None:
        """Send to one socket, serialized with every other send to it."""
        lock = self._send_locks.setdefault(ws, asyncio.Lock())
        async with lock:
            await ws.send_json(event)

    async def unregister(self, user_id: uuid.UUID, ws: WebSocket) -> None:
        async with self._lock:
            self._send_locks.pop(ws, None)
            conns = self._conns.get(user_id)
            if conns is not None:
                conns.discard(ws)
                if not conns:
                    self._conns.pop(user_id, None)

    async def send_to_users(self, user_ids: list[uuid.UUID], event: dict[str, Any]) -> None:
        """Send ``event`` (a JSON-able envelope) to every live connection of each
        user. Failed sockets are dropped; delivery is best-effort (clients
        reconcile missed events via REST forward-sync on reconnect)."""
        async with self._lock:
            targets = [(uid, ws) for uid in set(user_ids) for ws in self._conns.get(uid, set())]
        for uid, ws in targets:
            try:
                await self.send(ws, event)
            except Exception:  # noqa: BLE001 - a dead socket shouldn't break fan-out
                await self.unregister(uid, ws)

    def is_online(self, user_id: uuid.UUID) -> bool:
        return bool(self._conns.get(user_id))


_hub = Hub()


def get_hub() -> Hub:
    """Return the process-global hub."""
    return _hub
