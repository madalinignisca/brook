"""JanusClient against a scripted fake Janus WebSocket server.

Guards the bug the first real call exposed: Janus answers `trickle` and
`keepalive` with ONLY an `ack`. The client used to wait past the ack for a reply
that never comes, so every ICE candidate blocked its socket for the whole request
timeout and every keepalive "failed", letting Janus reap the session a minute
into every call.
"""

from __future__ import annotations

import asyncio
import json
import time

import websockets

from app import janus as janus_mod
from app.janus import JanusClient


async def _fake_janus(ws: websockets.ServerConnection) -> None:
    async for raw in ws:
        req = json.loads(raw)
        tx, kind = req["transaction"], req["janus"]
        if kind == "create":
            await ws.send(json.dumps({"janus": "success", "transaction": tx, "data": {"id": 1}}))
        elif kind in ("trickle", "keepalive"):
            await ws.send(json.dumps({"janus": "ack", "transaction": tx}))  # and nothing else
        elif kind == "message":
            await ws.send(json.dumps({"janus": "ack", "transaction": tx}))
            await asyncio.sleep(0.05)
            await ws.send(
                json.dumps(
                    {
                        "janus": "event",
                        "transaction": tx,
                        "sender": 7,
                        "plugindata": {
                            "plugin": "janus.plugin.videoroom",
                            "data": {"videoroom": "event"},
                        },
                    }
                )
            )


async def test_ack_only_requests_resolve_on_the_ack(monkeypatch) -> None:  # type: ignore[no-untyped-def]
    monkeypatch.setattr(janus_mod, "REQUEST_TIMEOUT_S", 2.0)
    async with websockets.serve(
        _fake_janus, "127.0.0.1", 0, subprotocols=["janus-protocol"]
    ) as srv:
        port = srv.sockets[0].getsockname()[1]
        client = JanusClient(f"ws://127.0.0.1:{port}", "secret")
        start = time.monotonic()
        await client.trickle(1, 7, {"candidate": "c", "sdpMid": "0", "sdpMLineIndex": 0})
        await client.trickle(1, 7, None)
        await client.request({"janus": "keepalive", "session_id": 1}, ack_is_final=True)
        assert time.monotonic() - start < 1.0, "ack-only requests must not wait for a reply"

        # ...while a plugin message still waits past its ack for the real event.
        msg = await client.message(1, 7, {"request": "configure"})
        assert msg["janus"] == "event"
