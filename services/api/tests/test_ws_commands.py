"""/ws beyond Phase 1 fan-out: close reasons, re-auth, expiry, the command layer.

Wire (PROTOCOL.md §2): auth {access_token} -> ready; every failure closes 1008
with a reason; commands carry id and get exactly one reply with re.
"""

from __future__ import annotations

import contextlib
import time
import uuid

import anyio
import jwt
import pytest
from fastapi.testclient import TestClient
from starlette.websockets import WebSocketDisconnect

from app.routers import ws as wsmod

AUTH = "/api/v1/auth"
KEY = "test-signing-key-at-least-32-bytes-long!"


def _token(tc: TestClient, handle: str = "alice", admin: str | None = None) -> str:
    hdr = {"Authorization": f"Bearer {admin}"} if admin else {}
    body = {"handle": handle, "display_name": handle.title(), "password": "supersecret"}
    assert tc.post(f"{AUTH}/register", json=body, headers=hdr).status_code == 201
    r = tc.post(f"{AUTH}/login", json={"handle": handle, "password": "supersecret"})
    return str(r.json()["access_token"])


# Short-lived test tokens expire at int(now) + 2, never + 1: exp has one-second
# granularity, so "+ 1" can leave ~0 s and the token may already be dead when the
# auth frame is checked (a flaky "auth_failed" instead of the behaviour under test).
def _forge(token: str, **claims: object) -> str:
    base = jwt.decode(token, options={"verify_signature": False})
    return jwt.encode({**base, **claims}, KEY, algorithm="HS256")


def _expect_close(ws, reason: str) -> None:  # type: ignore[no-untyped-def]
    with pytest.raises(WebSocketDisconnect) as e:
        ws.receive_json()
    assert (e.value.code, e.value.reason) == (wsmod.CLOSE_POLICY, reason)


def test_ready_carries_re_when_auth_has_id(sync_client: TestClient) -> None:
    tok = _token(sync_client)
    with sync_client.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "id": "a1", "data": {"access_token": tok}})
        r = ws.receive_json()
        assert (r["type"], r["re"]) == ("ready", "a1")
        uuid.UUID(r["data"]["user_id"])
        assert {"id", "ts"} <= r.keys()


@pytest.mark.parametrize(
    "first",
    [
        {"type": "ping", "id": "x", "data": {}},
        {"type": "auth", "data": {"access_token": "not-a-jwt"}},
        {"type": "auth", "data": {}},
        {"type": "auth", "data": {"token": "wrong-key-name"}},
    ],
)
def test_bad_first_frame_closes_1008_auth_failed(sync_client: TestClient, first: dict) -> None:  # type: ignore[type-arg]
    with sync_client.websocket_connect("/ws") as ws:
        ws.send_json(first)
        _expect_close(ws, "auth_failed")


def test_non_access_jwt_is_rejected(sync_client: TestClient) -> None:
    """A signed JWT of another type (e.g. the planned totp_pending) must not open
    a socket, or a half-finished 2FA login would get realtime access."""
    forged = _forge(_token(sync_client), type="totp_pending")
    with sync_client.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": forged}})
        _expect_close(ws, "auth_failed")


def test_auth_timeout(sync_client: TestClient, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(wsmod, "AUTH_TIMEOUT_S", 0.2)
    with sync_client.websocket_connect("/ws") as ws:
        _expect_close(ws, "auth_timeout")


def test_commands_unknown_garbage_and_ping(sync_client: TestClient) -> None:
    tok = _token(sync_client)
    with sync_client.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": tok}})
        ws.receive_json()
        ws.send_json({"type": "ping", "id": "p1", "data": {}})
        assert ws.receive_json()["re"] == "p1"
        ws.send_json({"type": "no.such", "id": "u1", "data": {}})
        err = ws.receive_json()
        assert (err["type"], err["re"], err["data"]["code"]) == ("error", "u1", "invalid")
        ws.send_text("{not json")
        assert ws.receive_json()["data"]["code"] == "invalid"
        ws.send_json({"type": "ping", "id": "p2", "data": {}})  # socket survived
        assert ws.receive_json()["re"] == "p2"


def test_oversized_frame_closes_1009(sync_client: TestClient) -> None:
    tok = _token(sync_client)
    with sync_client.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": tok}})
        ws.receive_json()
        ws.send_text("x" * (wsmod.MAX_FRAME_BYTES + 1))
        with pytest.raises(WebSocketDisconnect) as e:
            ws.receive_json()
        assert e.value.code == wsmod.CLOSE_TOO_LARGE


def test_socket_closes_when_token_expires(sync_client: TestClient) -> None:
    """Real token expiring in 1-2 s. After the deadline a ping must NOT get a pong:
    a regression fails fast here instead of hanging on a receive."""
    short = _forge(_token(sync_client), exp=int(time.time()) + 2)
    with sync_client.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": short}})
        assert ws.receive_json()["type"] == "ready"
        time.sleep(3.2)
        with contextlib.suppress(anyio.ClosedResourceError):
            ws.send_json({"type": "ping", "id": "late", "data": {}})
        _expect_close(ws, "token_expired")


def test_reauth_extends_the_socket(sync_client: TestClient) -> None:
    tok = _token(sync_client)
    short = _forge(tok, exp=int(time.time()) + 2)
    with sync_client.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": short}})
        ws.receive_json()
        ws.send_json({"type": "auth", "id": "r1", "data": {"access_token": tok}})
        r = ws.receive_json()
        assert (r["type"], r["re"]) == ("ready", "r1")
        time.sleep(3.2)  # past the first token's expiry
        ws.send_json({"type": "ping", "id": "alive", "data": {}})
        assert ws.receive_json()["re"] == "alive"


def test_reauth_as_another_user_closes(sync_client: TestClient) -> None:
    alice = _token(sync_client, "alice")
    bob = _token(sync_client, "bob", admin=alice)
    with sync_client.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": alice}})
        ws.receive_json()
        ws.send_json({"type": "auth", "id": "r", "data": {"access_token": bob}})
        _expect_close(ws, "auth_failed")
