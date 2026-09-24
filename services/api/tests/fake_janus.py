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
        # subscriber handle -> subscribed (feed, mid), like Janus multistream
        self.subs: dict[int, set[tuple[int, str]]] = {}
        self.pub_feed: dict[int, int] = {}  # publisher handle -> its feed id
        self.feed_streams: dict[int, dict[str, str]] = {}  # feed -> {mid: kind}, from configure
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
        """An offer for exactly the subscribed (feed, mid) streams; mids are this
        subscriber's own (0, 1, 2 ...), feed_mid is the publisher's."""
        streams = []
        for i, (feed, mid) in enumerate(sorted(self.subs[hid])):
            kind = self.feed_streams.get(feed, {}).get(mid, "video")
            streams.append(
                {"mid": str(i), "type": kind, "feed_id": feed, "feed_mid": mid, "active": True}
            )
        return {
            "jsep": {"type": "offer", "sdp": f"v=0 fake-offer {sorted(self.subs[hid])}"},
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
            feed = next(self._ids)
            self.pub_feed[hid] = feed
            data = {"videoroom": "joined", "id": feed, "private_id": next(self._ids)}
            return {"plugindata": {"data": data}}
        if req == "join" and body.get("ptype") == "subscriber":
            self.subs[hid] = {(s["feed"], s["mid"]) for s in body["streams"]}
            return self._offer(hid)
        if req == "update":
            subs = self.subs.setdefault(hid, set())
            subs |= {(s["feed"], s["mid"]) for s in body.get("subscribe", [])}
            subs -= {(s["feed"], s["mid"]) for s in body.get("unsubscribe", [])}
            return self._offer(hid)
        if req == "configure":
            from app.calls import _parse_mlines  # the server's own parser, for the kinds

            if jsep is not None and hid in self.pub_feed:
                self.feed_streams[self.pub_feed[hid]] = {
                    m.mid: m.kind for m in _parse_mlines(jsep["sdp"]) if m.active
                }
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
