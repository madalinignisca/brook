"""Call flows in-process against a fake Janus: publish, subscribe offers and
answers, renegotiation on publish/leave, ICE relay, mute, resume replay, grace
expiry, SFU loss, channel delete, the connect snapshot, and failure paths.

Reads are bounded: `collect()` sends a ping fence and gathers every frame up to
its pong, so a missing event fails an assert instead of hanging CI.
"""

from __future__ import annotations

import time
from collections.abc import Iterator
from contextlib import contextmanager
from typing import Any

import pytest
from fastapi.testclient import TestClient

from app import calls

from .fake_janus import FakeJanus

AUTH = "/api/v1/auth"
SDP_AV = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\n"


@pytest.fixture
def fake(sync_client: TestClient) -> Iterator[FakeJanus]:
    mgr = calls.manager
    f = FakeJanus()
    mgr._janus = f  # type: ignore[assignment]
    yield f
    for call in list(mgr.by_id.values()):
        for p in call.participants.values():
            if p.grace is not None:
                p.grace.cancel()
    mgr._janus = None
    mgr.by_id.clear()
    mgr.by_channel.clear()
    mgr._channel_locks.clear()


def _login(tc: TestClient, handle: str, admin: str | None = None) -> str:
    hdr = {"Authorization": f"Bearer {admin}"} if admin else {}
    body = {"handle": handle, "display_name": handle.title(), "password": "supersecret"}
    assert tc.post(f"{AUTH}/register", json=body, headers=hdr).status_code == 201
    r = tc.post(f"{AUTH}/login", json={"handle": handle, "password": "supersecret"})
    return str(r.json()["access_token"])


@contextmanager
def _ws(tc: TestClient, token: str) -> Iterator[Any]:
    with tc.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": token}})
        assert ws.receive_json()["type"] == "ready"
        yield ws


_fence = iter(range(10**9))


def collect(ws: Any, wait: float = 0.3) -> list[dict[str, Any]]:
    """Every frame the server sends up to a ping fence (after a short wait for
    background tasks)."""
    time.sleep(wait)
    fid = f"fence-{next(_fence)}"
    ws.send_json({"type": "ping", "id": fid, "data": {}})
    out = []
    while True:
        f: dict[str, Any] = ws.receive_json()
        if f.get("re") == fid:
            return out
        out.append(f)


def cmd(ws: Any, type_: str, data: dict[str, Any]) -> dict[str, Any]:
    rid = f"c-{next(_fence)}"
    ws.send_json({"type": type_, "id": rid, "data": data})
    while True:
        f: dict[str, Any] = ws.receive_json()
        if f.get("re") == rid:
            return f


def of(frames: list[dict[str, Any]], type_: str) -> list[dict[str, Any]]:
    return [f["data"] for f in frames if f["type"] == type_]


def _setup(tc: TestClient) -> tuple[str, str, str, str]:
    a = _login(tc, "alice")
    b = _login(tc, "bob", admin=a)
    c = _login(tc, "carol", admin=a)
    hdr = {"Authorization": f"Bearer {a}"}
    ch = tc.post("/api/v1/channels", json={"kind": "channel", "name": "x"}, headers=hdr).json()
    for h in ("bob", "carol"):
        tc.post(f"/api/v1/channels/{ch['id']}/members", json={"handle": h}, headers=hdr)
    return a, b, c, str(ch["id"])


def _participant(call_id: str, pid: str) -> calls.Participant:
    return calls.manager.by_id[call_id].participants[pid]


def _publish_live(tc: TestClient, fake: FakeJanus, ws: Any, call_id: str, pid: str) -> None:
    r = cmd(ws, "call.publish", {"call_id": call_id, "sdp": SDP_AV})
    assert r["type"] == "call.publish.answer" and r["data"]["sdp"] == "v=0 fake-answer"
    tc.portal.call(fake.fire, _participant(call_id, pid).pub_hid, {"janus": "webrtcup"})


def test_full_call_flow(sync_client: TestClient, fake: FakeJanus) -> None:
    a, b, _c, ch = _setup(sync_client)
    with _ws(sync_client, a) as wa, _ws(sync_client, b) as wb:
        ja = cmd(wa, "call.join", {"channel_id": ch})["data"]
        call_id, pa = ja["call_id"], ja["self"]["participant_id"]
        assert ja["participants"] == []
        jb = cmd(wb, "call.join", {"channel_id": ch})["data"]
        pb = jb["self"]["participant_id"]
        assert [p["participant_id"] for p in jb["participants"]] == [pa]
        joined = of(collect(wa), "call.participant")
        assert joined[-1]["event"] == "joined" and joined[-1]["participant"]["audio"] is False

        # alice publishes. Until Janus reports her PC up, nobody may subscribe to
        # her: Janus answers "No such feed" (the race the real-browser E2E found).
        r = cmd(wa, "call.publish", {"call_id": call_id, "sdp": SDP_AV})
        assert r["type"] == "call.publish.answer" and r["data"]["sdp"] == "v=0 fake-answer"
        early = collect(wb)
        assert of(early, "call.subscribe.offer") == [], "offered before the publisher was live"
        # ...after webrtcup bob is offered alice's two streams
        sync_client.portal.call(fake.fire, _participant(call_id, pa).pub_hid, {"janus": "webrtcup"})
        frames = early + collect(wb)
        upd = of(frames, "call.participant")
        assert upd and upd[0]["event"] == "updated"
        assert upd[0]["participant"]["publishing"] == [
            {"kind": "audio", "source": "mic"},
            {"kind": "video", "source": "camera"},
        ]
        assert upd[0]["participant"]["audio"] is True
        offer = of(frames, "call.subscribe.offer")[-1]
        assert offer["version"] == 1
        assert {(s["mid"], s["participant_id"], s["kind"]) for s in offer["streams"]} == {
            ("0", pa, "audio"),
            ("1", pa, "video"),
        }

        # a stale version is refused; the current one is accepted
        assert (
            cmd(wb, "call.subscribe.answer", {"call_id": call_id, "version": 0, "sdp": "x"})[
                "data"
            ]["code"]
            == "stale"
        )
        ok = cmd(wb, "call.subscribe.answer", {"call_id": call_id, "version": 1, "sdp": "ans"})
        assert ok["type"] == "call.ok"

        # bob publishes too: alice gets an offer with bob's streams
        _publish_live(sync_client, fake, wb, call_id, pb)
        offer_a = of(collect(wa), "call.subscribe.offer")[-1]
        assert {s["participant_id"] for s in offer_a["streams"]} == {pb}

        # ICE is relayed to the right handle, including end-of-candidates
        cand = {
            "candidate": "candidate:1 1 udp 1 10.0.0.1 9 typ host",
            "sdpMid": "0",
            "sdpMLineIndex": 0,
        }
        wb.send_json(
            {
                "type": "call.ice",
                "id": "i1",
                "data": {"call_id": call_id, "pc": "publish", "candidate": cand},
            }
        )
        wb.send_json(
            {
                "type": "call.ice",
                "id": "i2",
                "data": {"call_id": call_id, "pc": "subscribe", "candidate": None},
            }
        )
        collect(wb, wait=0.1)
        trickles = [r for n, r in fake.requests if n == "trickle"]
        assert trickles[-2]["handle"] == _participant(call_id, pb).pub_hid
        assert trickles[-1] == {"handle": _participant(call_id, pb).sub_hid, "candidate": None}

        # Janus-side trickle reaches the client as call.ice
        sync_client.portal.call(
            fake.fire,
            _participant(call_id, pb).pub_hid,
            {"janus": "trickle", "candidate": {"completed": True}},
        )
        ice = of(collect(wb), "call.ice")
        assert ice and ice[-1] == {"call_id": call_id, "pc": "publish", "candidate": None}

        # mute is broadcast
        assert (
            cmd(wb, "call.media", {"call_id": call_id, "audio": False, "video": True})["type"]
            == "call.ok"
        )
        muted = of(collect(wa), "call.participant")[-1]["participant"]
        assert (muted["audio"], muted["video"]) == (False, True)

        # alice answers her pending offer, then leaves: bob sees `left` + a re-offer
        cmd(
            wa,
            "call.subscribe.answer",
            {"call_id": call_id, "version": offer_a["version"], "sdp": "ans"},
        )
        assert cmd(wa, "call.leave", {"call_id": call_id})["type"] == "call.ok"
        frames = collect(wb)
        assert of(frames, "call.participant")[-1]["event"] == "left"
        assert of(frames, "call.subscribe.offer")[-1]["streams"] == []
        # alice's commands are now not_in_call
        assert (
            cmd(wa, "call.media", {"call_id": call_id, "audio": True, "video": True})["data"][
                "code"
            ]
            == "not_in_call"
        )


def test_resume_replays_the_unanswered_offer(sync_client: TestClient, fake: FakeJanus) -> None:
    a, b, _c, ch = _setup(sync_client)
    with _ws(sync_client, a) as wa:
        ja = cmd(wa, "call.join", {"channel_id": ch})["data"]
        call_id = ja["call_id"]
        with _ws(sync_client, b) as wb:
            jb = cmd(wb, "call.join", {"channel_id": ch})["data"]
            _publish_live(sync_client, fake, wa, call_id, ja["self"]["participant_id"])
            first = of(collect(wb), "call.subscribe.offer")[-1]
        # bob's socket dropped with the offer unanswered; he resumes
        with _ws(sync_client, b) as wb2:
            r = cmd(
                wb2,
                "call.resume",
                {
                    "call_id": call_id,
                    "participant_id": jb["self"]["participant_id"],
                    "resume_token": jb["self"]["resume_token"],
                },
            )
            assert r["type"] == "call.joined"
            replay = of(collect(wb2), "call.subscribe.offer")
            assert replay and replay[-1]["version"] == first["version"]


def test_grace_expiry_removes_a_vanished_participant(
    sync_client: TestClient, fake: FakeJanus, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(calls, "RESUME_GRACE_S", 0.2)
    a, b, _c, ch = _setup(sync_client)
    with _ws(sync_client, a) as wa:
        call_id = cmd(wa, "call.join", {"channel_id": ch})["data"]["call_id"]
        with _ws(sync_client, b) as wb:
            cmd(wb, "call.join", {"channel_id": ch})
        frames = collect(wa, wait=0.6)
        assert "left" in [p["event"] for p in of(frames, "call.participant")]
        assert len(calls.manager.by_id[call_id].participants) == 1


def test_sfu_loss_ends_every_call(sync_client: TestClient, fake: FakeJanus) -> None:
    a, _b, _c, ch = _setup(sync_client)
    with _ws(sync_client, a) as wa:
        call_id = cmd(wa, "call.join", {"channel_id": ch})["data"]["call_id"]
        sync_client.portal.call(calls.manager._on_janus_lost)
        frames = collect(wa)
        assert {"call_id": call_id, "reason": "sfu_restart"} in of(frames, "call.ended")
        assert of(frames, "channel.call")[-1]["call_id"] is None
        assert call_id not in calls.manager.by_id


def test_channel_delete_ends_the_call(sync_client: TestClient, fake: FakeJanus) -> None:
    a, _b, _c, ch = _setup(sync_client)
    with _ws(sync_client, a) as wa:
        call_id = cmd(wa, "call.join", {"channel_id": ch})["data"]["call_id"]
        r = sync_client.delete(f"/api/v1/channels/{ch}", headers={"Authorization": f"Bearer {a}"})
        assert r.status_code == 204
        assert {"call_id": call_id, "reason": "removed"} in of(collect(wa), "call.ended")


def test_connect_snapshot_lists_calls_in_progress(sync_client: TestClient, fake: FakeJanus) -> None:
    a, _b, c, ch = _setup(sync_client)
    with _ws(sync_client, a) as wa:
        call_id = cmd(wa, "call.join", {"channel_id": ch})["data"]["call_id"]
        with sync_client.websocket_connect("/ws") as wc:
            wc.send_json({"type": "auth", "data": {"access_token": c}})
            assert wc.receive_json()["type"] == "ready"
            snap = wc.receive_json()
            assert (snap["type"], snap["data"]["call_id"]) == ("channel.call", call_id)
            assert snap["data"]["participant_count"] == 1


def test_rejected_answer_resets_the_subscribe_pc(sync_client: TestClient, fake: FakeJanus) -> None:
    a, b, _c, ch = _setup(sync_client)
    with _ws(sync_client, a) as wa, _ws(sync_client, b) as wb:
        ja = cmd(wa, "call.join", {"channel_id": ch})["data"]
        cmd(wb, "call.join", {"channel_id": ch})
        _publish_live(sync_client, fake, wa, ja["call_id"], ja["self"]["participant_id"])
        v = of(collect(wb), "call.subscribe.offer")[-1]["version"]
        fake.fail.add("start")
        r = cmd(wb, "call.subscribe.answer", {"call_id": ja["call_id"], "version": v, "sdp": "x"})
        assert (r["type"], r["data"]["code"]) == ("error", "invalid")
        assert "fake" not in r["data"]["message"]  # Janus text stays in the log
        fake.fail.discard("start")
        again = of(collect(wb), "call.subscribe.offer")
        assert again and again[-1]["version"] == v + 1  # a fresh offer, not wedged


@pytest.mark.parametrize("failing", ["create", "join"])
def test_sfu_refusals_leak_nothing(sync_client: TestClient, fake: FakeJanus, failing: str) -> None:
    a, _b, _c, ch = _setup(sync_client)
    fake.fail.add(failing)
    with _ws(sync_client, a) as wa:
        r = cmd(wa, "call.join", {"channel_id": ch})
        assert (r["type"], r["data"]["code"]) == ("error", "sfu_unavailable")
    assert not calls.manager.by_id and not calls.manager.by_channel
    assert fake.destroyed, "the Janus session created before the failure must be destroyed"


def test_archived_channel_refuses_calls(sync_client: TestClient, fake: FakeJanus) -> None:
    a, _b, _c, ch = _setup(sync_client)
    r = sync_client.patch(
        f"/api/v1/channels/{ch}", json={"archived": True}, headers={"Authorization": f"Bearer {a}"}
    )
    assert r.status_code == 200, r.text
    with _ws(sync_client, a) as wa:
        r2 = cmd(wa, "call.join", {"channel_id": ch})
        assert (r2["type"], r2["data"]["code"]) == ("error", "bad_state")
