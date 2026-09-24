"""Call commands without an SFU: registration, validation, authorization.

The media path is covered end to end by e2e/call_e2e.py (real Janus + Chrome).
These guard what can be checked in-process, including the regression where the
call handlers were silently never registered: every call.* command answered
"unknown type" while every other test stayed green.

Every socket is opened with ``with``: a session entered by hand and never exited
makes the TestClient wait forever at teardown (that hung this file once).
"""

from __future__ import annotations

from collections.abc import Iterator
from contextlib import contextmanager
from typing import Any

import pytest
from fastapi.testclient import TestClient

from app.routers import ws as wsmod

AUTH = "/api/v1/auth"


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


def _channel(tc: TestClient, token: str) -> str:
    r = tc.post(
        "/api/v1/channels",
        json={"kind": "channel", "name": "calls"},
        headers={"Authorization": f"Bearer {token}"},
    )
    assert r.status_code == 201, r.text
    return str(r.json()["id"])


@pytest.mark.parametrize(
    "type_",
    [
        "call.join",
        "call.publish",
        "call.subscribe.answer",
        "call.ice",
        "call.media",
        "call.leave",
        "call.resume",
    ],
)
def test_call_commands_are_registered(sync_client: TestClient, type_: str) -> None:
    assert type_ in wsmod._handlers, f"{type_} has no WebSocket handler"


def test_join_non_member_is_refused(sync_client: TestClient) -> None:
    admin = _login(sync_client, "alice")
    other = _login(sync_client, "bob", admin=admin)
    ch = _channel(sync_client, admin)
    with _ws(sync_client, other) as ws:
        ws.send_json({"type": "call.join", "id": "j", "data": {"channel_id": ch}})
        r = ws.receive_json()
        assert (r["type"], r["re"], r["data"]["code"]) == ("error", "j", "not_member")


def test_join_member_without_sfu_is_sfu_unavailable(sync_client: TestClient) -> None:
    admin = _login(sync_client, "alice")
    ch = _channel(sync_client, admin)
    with _ws(sync_client, admin) as ws:
        ws.send_json({"type": "call.join", "id": "j", "data": {"channel_id": ch}})
        r = ws.receive_json()
        assert (r["type"], r["re"], r["data"]["code"]) == ("error", "j", "sfu_unavailable")


@pytest.mark.parametrize(
    ("type_", "data", "code"),
    [
        ("call.join", {}, "invalid"),
        ("call.join", {"channel_id": "not-a-uuid"}, "invalid"),
        ("call.publish", {"call_id": "nope", "sdp": "v=0"}, "not_in_call"),
        ("call.leave", {"call_id": "nope"}, "not_in_call"),
        (
            "call.resume",
            {"call_id": "x", "participant_id": "p", "resume_token": "t"},
            "not_in_call",
        ),
        ("call.resume", {"call_id": "nope"}, "invalid"),
    ],
)
def test_bad_commands_get_exactly_one_error(
    sync_client: TestClient, type_: str, data: dict[str, Any], code: str
) -> None:
    with _ws(sync_client, _login(sync_client, "alice")) as ws:
        ws.send_json({"type": type_, "id": "c1", "data": data})
        r = ws.receive_json()
        assert (r["type"], r["re"], r["data"]["code"]) == ("error", "c1", code)
        # exactly one reply: the next frame answers our ping, nothing in between
        ws.send_json({"type": "ping", "id": "p", "data": {}})
        assert ws.receive_json()["re"] == "p"


def test_resume_token_lookback() -> None:
    """Current and last-used tokens resume; anything older, or garbage, does not."""
    from app.calls import _token_ok

    assert _token_ok("cur", "cur", None)
    assert _token_ok("prev", "cur", "prev")  # reply to the last resume was lost
    assert not _token_ok("older", "cur", "prev")
    assert not _token_ok("", "cur", None)
