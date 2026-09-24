"""Participant binding and resume authorization, with a fake in-process Janus.

The auth review showed these checks had no test that could go red: deleting the
socket-ownership check in _participant_of, or either resume check, left the whole
suite green. Each test here was observed failing with its guarded line removed.

The fake Janus answers just enough of the VideoRoom API for joins to succeed.
Media is covered for real by e2e/call_e2e.py.
"""

from __future__ import annotations

from collections.abc import Iterator
from contextlib import contextmanager
from typing import Any

import pytest
from fastapi.testclient import TestClient

from app import calls

from .fake_janus import FakeJanus

AUTH = "/api/v1/auth"


@pytest.fixture
def fake_janus(sync_client: TestClient) -> Iterator[None]:
    mgr = calls.manager
    mgr._janus = FakeJanus()  # type: ignore[assignment]
    yield
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


def _reply(ws: Any, re: str) -> dict[str, Any]:
    """Skip unsolicited events until the reply to command `re`."""
    while True:
        frame: dict[str, Any] = ws.receive_json()
        if frame.get("re") == re:
            return frame


def _setup(tc: TestClient) -> tuple[str, str, str, str]:
    """alice (admin), bob, carol; a channel with all three. Returns tokens + channel id."""
    a = _login(tc, "alice")
    b = _login(tc, "bob", admin=a)
    c = _login(tc, "carol", admin=a)
    hdr = {"Authorization": f"Bearer {a}"}
    ch = tc.post("/api/v1/channels", json={"kind": "channel", "name": "x"}, headers=hdr).json()
    for h in ("bob", "carol"):
        assert (
            tc.post(
                f"/api/v1/channels/{ch['id']}/members", json={"handle": h}, headers=hdr
            ).status_code
            == 204
        )
    return a, b, c, str(ch["id"])


def _join(ws: Any, channel: str) -> dict[str, Any]:
    ws.send_json({"type": "call.join", "id": "j", "data": {"channel_id": channel}})
    r = _reply(ws, "j")
    assert r["type"] == "call.joined", r
    return r["data"]


def test_commands_act_only_on_the_senders_own_participant(
    sync_client: TestClient, fake_janus: None
) -> None:
    """call_id is broadcast to every member, so it must grant nothing: commands
    resolve the participant by the sending socket. (Guards `p.conn is conn`.)"""
    a, b, c, ch = _setup(sync_client)
    with _ws(sync_client, a) as wa, _ws(sync_client, b) as wb, _ws(sync_client, c) as wc:
        ja = _join(wa, ch)
        jb = _join(wb, ch)
        call_id = ja["call_id"]
        # bob leaves: bob must go, alice must stay
        wb.send_json({"type": "call.leave", "id": "l", "data": {"call_id": call_id}})
        assert _reply(wb, "l")["type"] == "call.ok"
        wb.send_json({"type": "ping", "id": "sync", "data": {}})
        _reply(wb, "sync")
        remaining = set(calls.manager.by_id[call_id].participants)
        assert remaining == {ja["self"]["participant_id"]}, remaining
        assert jb["self"]["participant_id"] not in remaining
        # carol knows call_id but never joined: nothing works for her
        wc.send_json(
            {"type": "call.publish", "id": "p", "data": {"call_id": call_id, "sdp": "v=0"}}
        )
        r = _reply(wc, "p")
        assert (r["type"], r["data"]["code"]) == ("error", "not_in_call")


def test_same_socket_cannot_join_twice(sync_client: TestClient, fake_janus: None) -> None:
    a, _b, _c, ch = _setup(sync_client)
    with _ws(sync_client, a) as wa:
        _join(wa, ch)
        wa.send_json({"type": "call.join", "id": "j2", "data": {"channel_id": ch}})
        r = _reply(wa, "j2")
        assert (r["type"], r["data"]["code"]) == ("error", "bad_state")


def test_resume_requires_same_user_and_valid_token(
    sync_client: TestClient, fake_janus: None
) -> None:
    a, b, _c, ch = _setup(sync_client)
    with _ws(sync_client, a) as wa:
        me = _join(wa, ch)
    call_id, pid, tok = me["call_id"], me["self"]["participant_id"], me["self"]["resume_token"]

    def resume(token: str, as_: str, rid: str) -> dict[str, Any]:
        with _ws(sync_client, as_) as w:
            w.send_json(
                {
                    "type": "call.resume",
                    "id": rid,
                    "data": {"call_id": call_id, "participant_id": pid, "resume_token": token},
                }
            )
            return _reply(w, rid)

    # right participant, wrong token
    r = resume("wrong-token", a, "r1")
    assert (r["type"], r["data"]["code"]) == ("error", "not_in_call")
    # right token, different user
    r = resume(tok, b, "r2")
    assert (r["type"], r["data"]["code"]) == ("error", "not_in_call")
    # a non-ASCII token is a clean not_in_call, not an internal error
    r = resume("tökén", a, "r3")
    assert (r["type"], r["data"]["code"]) == ("error", "not_in_call")
    # right user + right token works, and rotates the token
    r = resume(tok, a, "r4")
    assert r["type"] == "call.joined"
    assert r["data"]["self"]["resume_token"] != tok


def test_resume_notifies_the_displaced_socket(sync_client: TestClient, fake_janus: None) -> None:
    """Bounded read: a ping on the old socket after the takeover; every frame up to
    its pong is collected. A missing notice fails the assert instead of hanging."""
    import time

    a, _b, _c, ch = _setup(sync_client)
    with _ws(sync_client, a) as old:
        me = _join(old, ch)
        with _ws(sync_client, a) as new:
            new.send_json(
                {
                    "type": "call.resume",
                    "id": "r",
                    "data": {
                        "call_id": me["call_id"],
                        "participant_id": me["self"]["participant_id"],
                        "resume_token": me["self"]["resume_token"],
                    },
                }
            )
            assert _reply(new, "r")["type"] == "call.joined"
            time.sleep(0.3)  # the notice is sent by a background task
            old.send_json({"type": "ping", "id": "fence", "data": {}})
            seen = []
            while True:
                f = old.receive_json()
                if f.get("re") == "fence":
                    break
                seen.append((f["type"], f["data"].get("reason")))
        assert ("call.ended", "replaced") in seen, seen
