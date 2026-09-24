"""A fake JanusClient for in-process call tests.

Answers the VideoRoom requests calls.py makes the way Janus 1.4 does, records
every request, and lets a test fire the events Janus sends on its own
(``webrtcup`` on a publish handle, ``trickle``). Media itself is covered for
real by e2e/call_e2e.py.
"""

from __future__ import annotations

import itertools
from typing import Any

from app.janus import JanusError


class FakeJanus:
    def __init__(self) -> None:
        self._ids = itertools.count(1000)
        self.listeners: dict[int, Any] = {}
        self.requests: list[tuple[str, dict[str, Any]]] = []
        self.destroyed: list[int] = []
        self.fail: set[str] = set()  # request names that raise JanusError
        self.subs: dict[int, set[int]] = {}  # subscriber handle -> feeds
        self.on_disconnect: Any = None

    async def create_session(self) -> int:
        if "create_session" in self.fail:
            raise JanusError("fake: no session")
        return next(self._ids)

    async def attach(self, sid: int, listener: Any = None) -> int:
        hid = next(self._ids)
        if listener is not None:
            self.listeners[hid] = listener
        return hid

    def _offer(self, hid: int) -> dict[str, Any]:
        feeds = sorted(self.subs[hid])
        streams = []
        for i, feed in enumerate(feeds):
            streams.append({"mid": str(2 * i), "type": "audio", "feed_id": feed, "active": True})
            streams.append(
                {"mid": str(2 * i + 1), "type": "video", "feed_id": feed, "active": True}
            )
        return {
            "jsep": {"type": "offer", "sdp": f"v=0 fake-offer feeds={feeds}"},
            "plugindata": {"data": {"videoroom": "attached", "streams": streams}},
        }

    async def message(
        self, sid: int, hid: int, body: dict[str, Any], jsep: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        req = body["request"]
        self.requests.append((req, body))
        if req in self.fail:
            raise JanusError(f"fake: {req} refused")
        if req == "join" and body.get("ptype") == "publisher":
            data = {"videoroom": "joined", "id": next(self._ids), "private_id": next(self._ids)}
            return {"plugindata": {"data": data}}
        if req == "join" and body.get("ptype") == "subscriber":
            self.subs[hid] = {s["feed"] for s in body["streams"]}
            return self._offer(hid)
        if req == "update":
            feeds = self.subs.setdefault(hid, set())
            feeds |= {s["feed"] for s in body.get("subscribe", [])}
            feeds -= {s["feed"] for s in body.get("unsubscribe", [])}
            return self._offer(hid)
        if req == "configure":
            return {
                "jsep": {"type": "answer", "sdp": "v=0 fake-answer"},
                "plugindata": {"data": {}},
            }
        return {"plugindata": {"data": {}}}

    async def trickle(self, sid: int, hid: int, candidate: Any) -> None:
        self.requests.append(("trickle", {"handle": hid, "candidate": candidate}))

    async def destroy_session(self, sid: int) -> None:
        self.destroyed.append(sid)

    async def detach(self, sid: int, hid: int) -> None:
        self.listeners.pop(hid, None)

    def detach_listener(self, hid: int) -> None:
        self.listeners.pop(hid, None)

    async def fire(self, hid: int, event: dict[str, Any]) -> None:
        """Deliver an unsolicited Janus event to a handle's listener."""
        await self.listeners[hid](event)
