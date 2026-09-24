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


async def _scripted(ws: websockets.ServerConnection) -> None:
    """Janus answering by request kind, plus unsolicited events and a hang-up."""
    async for raw in ws:
        req = json.loads(raw)
        tx, kind = req["transaction"], req["janus"]
        if req.get("apisecret") != "secret":
            await ws.send(
                json.dumps(
                    {
                        "janus": "error",
                        "transaction": tx,
                        "error": {"code": 403, "reason": "Unauthorized"},
                    }
                )
            )
        elif kind == "create":
            await ws.send(json.dumps({"janus": "success", "transaction": tx, "data": {"id": 11}}))
        elif kind == "attach":
            await ws.send(json.dumps({"janus": "success", "transaction": tx, "data": {"id": 22}}))
        elif kind in ("destroy", "detach"):
            await ws.send(json.dumps({"janus": "success", "transaction": tx}))
        elif kind == "message" and req["body"]["request"] == "bad":
            await ws.send(
                json.dumps(
                    {
                        "janus": "event",
                        "transaction": tx,
                        "plugindata": {
                            "plugin": "janus.plugin.videoroom",
                            "data": {"error": "No such feed"},
                        },
                    }
                )
            )
        elif kind == "message" and req["body"]["request"] == "events":
            await ws.send(json.dumps({"janus": "ack", "transaction": tx}))
            for n in range(3):  # unsolicited, in order, to handle 22
                await ws.send(json.dumps({"janus": "event", "sender": 22, "n": n}))
            await ws.send(
                json.dumps(
                    {
                        "janus": "event",
                        "transaction": tx,
                        "plugindata": {"plugin": "janus.plugin.videoroom", "data": {}},
                    }
                )
            )
        elif kind == "message" and req["body"]["request"] == "hangup":
            await ws.close()
            return


async def test_errors_listeners_and_connection_loss() -> None:
    async with websockets.serve(_scripted, "127.0.0.1", 0, subprotocols=["janus-protocol"]) as srv:
        port = srv.sockets[0].getsockname()[1]
        url = f"ws://127.0.0.1:{port}"

        # a wrong secret is a JanusError, not a silent success
        bad = JanusClient(url, "wrong")
        try:
            await bad.create_session()
            raise AssertionError("expected JanusError")
        except janus_mod.JanusError:
            pass

        client = JanusClient(url, "secret")
        sid = await client.create_session()
        seen: list[int] = []

        async def listener(msg: dict) -> None:  # type: ignore[type-arg]
            await asyncio.sleep(0.01 * (3 - msg["n"]))  # later events finish faster...
            seen.append(msg["n"])

        hid = await client.attach(sid, listener)
        assert (sid, hid) == (11, 22)

        # a plugin-level error ({"error": ...} inside plugindata) raises
        try:
            await client.message(sid, hid, {"request": "bad"})
            raise AssertionError("expected JanusError")
        except janus_mod.JanusError as exc:
            assert "No such feed" in str(exc)

        # unsolicited events run off the read loop, yet in per-handle order
        await client.message(sid, hid, {"request": "events"})
        await asyncio.sleep(0.2)
        assert seen == [0, 1, 2]  # ...but order is kept

        client.detach_listener(hid)
        await client.detach(sid, hid)
        await client.destroy_session(sid)

        # Janus hanging up fails pending requests and fires on_disconnect
        lost = asyncio.Event()

        async def on_lost() -> None:
            lost.set()

        client.on_disconnect = on_lost
        await client.create_session()
        try:
            await client.message(11, 22, {"request": "hangup"})
            raise AssertionError("expected JanusError")
        except janus_mod.JanusError:
            pass
        await asyncio.wait_for(lost.wait(), 2)


async def test_unreachable_janus_is_a_janus_error() -> None:
    client = JanusClient("ws://127.0.0.1:9", "secret")  # nothing listens on port 9
    try:
        await client.create_session()
        raise AssertionError("expected JanusError")
    except janus_mod.JanusError as exc:
        assert "cannot reach Janus" in str(exc)
