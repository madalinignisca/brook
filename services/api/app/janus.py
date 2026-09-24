"""Minimal async client for the Janus WebSocket API (``janus-protocol``).

Only ``api`` talks to Janus (ARCHITECTURE.md §Signaling model); clients never see
Janus ids or messages. This is the smallest surface the call layer needs:
sessions, VideoRoom handles, plugin messages with an optional JSEP, and trickle.

Janus's asynchronous request pattern: a plugin ``message`` is first ``ack``ed and
the real answer arrives later as an ``event`` carrying the same ``transaction``.
But ``trickle`` and ``keepalive`` get **only** the ``ack``: for those the ack is the
final answer. Waiting past it for a reply that never comes made every trickled
candidate block its socket for the full request timeout, and made every keepalive
"fail", which let Janus reap the session a minute into every call.
Synchronous plugin requests (e.g. VideoRoom ``create``) answer with ``success``
directly. Events with no transaction (roster changes, hangups) go to the
per-handle listener registered with :meth:`JanusClient.on_event`.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import uuid
from collections.abc import Awaitable, Callable
from typing import Any

import websockets

log = logging.getLogger(__name__)

KEEPALIVE_S = 25.0  # Janus reaps idle sessions after session_timeout (60 s, janus.jcfg)
REQUEST_TIMEOUT_S = 10.0

EventListener = Callable[[dict[str, Any]], Awaitable[None]]


class JanusError(Exception):
    """Janus refused a request, or is unreachable. Maps to ``sfu_unavailable`` /
    ``invalid`` at the protocol level."""


class JanusClient:
    """One WebSocket to Janus, shared by every session ``api`` owns."""

    def __init__(self, url: str, api_secret: str) -> None:
        self._url = url
        self._secret = api_secret
        self._ws: Any = None
        self._reader: asyncio.Task[None] | None = None
        self._pending: dict[str, asyncio.Future[dict[str, Any]]] = {}
        self._ack_is_final: set[str] = set()  # transactions answered by their ack
        self._listeners: dict[int, EventListener] = {}
        self._keepalives: dict[int, asyncio.Task[None]] = {}
        self._connect_lock = asyncio.Lock()
        self.on_disconnect: Callable[[], Awaitable[None]] | None = None

    async def _ensure_connected(self) -> None:
        async with self._connect_lock:
            if self._ws is not None:
                return
            try:
                self._ws = await websockets.connect(
                    self._url, subprotocols=[websockets.Subprotocol("janus-protocol")]
                )
            except (OSError, websockets.WebSocketException) as exc:
                raise JanusError(f"cannot reach Janus: {exc}") from exc
            self._reader = asyncio.create_task(self._read_loop())

    async def _read_loop(self) -> None:
        try:
            async for raw in self._ws:
                msg = json.loads(raw)
                tx = msg.get("transaction", "")
                if msg.get("janus") == "ack" and tx not in self._ack_is_final:
                    continue  # async request accepted; the answer follows as an event
                self._ack_is_final.discard(tx)
                fut = self._pending.pop(tx, None)
                if fut is not None and not fut.done():
                    fut.set_result(msg)
                    continue
                sender = msg.get("sender")
                listener = self._listeners.get(sender) if isinstance(sender, int) else None
                if listener is not None:
                    try:
                        await listener(msg)
                    except Exception:
                        log.exception("janus event listener failed")
        except websockets.WebSocketException:
            log.warning("janus connection lost")
        finally:
            self._ws = None
            for fut in self._pending.values():
                if not fut.done():
                    fut.set_exception(JanusError("Janus connection lost"))
            self._pending.clear()
            # Every Janus session died with the socket: stop keepalives, drop
            # listeners, and let the call layer end the calls.
            for task in self._keepalives.values():
                task.cancel()
            self._keepalives.clear()
            self._listeners.clear()
            if self.on_disconnect is not None:
                with contextlib.suppress(Exception):
                    await self.on_disconnect()

    async def request(self, body: dict[str, Any], ack_is_final: bool = False) -> dict[str, Any]:
        """Send one request and wait for its final answer (the ack, if ``ack_is_final``)."""
        await self._ensure_connected()
        tx = uuid.uuid4().hex
        fut: asyncio.Future[dict[str, Any]] = asyncio.get_running_loop().create_future()
        self._pending[tx] = fut
        if ack_is_final:
            self._ack_is_final.add(tx)
        await self._ws.send(json.dumps({**body, "transaction": tx, "apisecret": self._secret}))
        try:
            msg = await asyncio.wait_for(fut, REQUEST_TIMEOUT_S)
        except TimeoutError as exc:
            self._pending.pop(tx, None)
            self._ack_is_final.discard(tx)
            raise JanusError(f"Janus timed out on {body.get('janus')}") from exc
        if msg.get("janus") == "error":
            raise JanusError(str(msg.get("error")))
        plugin_err = msg.get("plugindata", {}).get("data", {}).get("error")
        if plugin_err:
            raise JanusError(str(plugin_err))
        return msg

    async def create_session(self) -> int:
        sid = int((await self.request({"janus": "create"}))["data"]["id"])
        self._keepalives[sid] = asyncio.create_task(self._keepalive(sid))
        return sid

    async def _keepalive(self, sid: int) -> None:
        while True:
            await asyncio.sleep(KEEPALIVE_S)
            try:
                await self.request({"janus": "keepalive", "session_id": sid}, ack_is_final=True)
            except JanusError:
                return

    async def destroy_session(self, sid: int) -> None:
        task = self._keepalives.pop(sid, None)
        if task is not None:
            task.cancel()
        with contextlib.suppress(JanusError):
            await self.request({"janus": "destroy", "session_id": sid})

    async def attach(self, sid: int, listener: EventListener | None = None) -> int:
        msg = await self.request(
            {"janus": "attach", "session_id": sid, "plugin": "janus.plugin.videoroom"}
        )
        hid = int(msg["data"]["id"])
        if listener is not None:
            self._listeners[hid] = listener
        return hid

    def detach_listener(self, hid: int) -> None:
        self._listeners.pop(hid, None)

    async def detach(self, sid: int, hid: int) -> None:
        """Drop a handle (e.g. a subscriber whose first join failed)."""
        self._listeners.pop(hid, None)
        with contextlib.suppress(JanusError):
            await self.request({"janus": "detach", "session_id": sid, "handle_id": hid})

    async def message(
        self, sid: int, hid: int, body: dict[str, Any], jsep: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        """A VideoRoom request. Returns the whole Janus answer (``plugindata`` + ``jsep``)."""
        req: dict[str, Any] = {
            "janus": "message",
            "session_id": sid,
            "handle_id": hid,
            "body": body,
        }
        if jsep is not None:
            req["jsep"] = jsep
        return await self.request(req)

    async def trickle(self, sid: int, hid: int, candidate: dict[str, Any] | None) -> None:
        """Relay a client ICE candidate; ``None`` means end-of-candidates."""
        await self.request(
            {
                "janus": "trickle",
                "session_id": sid,
                "handle_id": hid,
                "candidate": candidate if candidate is not None else {"completed": True},
            },
            ack_is_final=True,
        )
