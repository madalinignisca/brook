"""Sign out everywhere: a password change (checkbox, default on) or an admin reset
revokes every other session at once, on REST and on open WebSockets, instead of
when the 15-minute access token expires."""

from __future__ import annotations

from datetime import UTC, datetime

import httpx
import pytest
from fastapi.testclient import TestClient
from starlette.websockets import WebSocketDisconnect

from app.models import User
from app.security import issued_at_ms

AUTH = "/api/v1/auth"
PW = "supersecret"


def _h(token: str) -> dict[str, str]:
    return {"Authorization": f"Bearer {token}"}


async def _pair(client: httpx.AsyncClient, handle: str, password: str = PW) -> dict[str, str]:
    r = await client.post(f"{AUTH}/login", json={"handle": handle, "password": password})
    assert r.status_code == 200, r.text
    return dict(r.json())


async def _setup(client: httpx.AsyncClient) -> tuple[dict[str, str], dict[str, str]]:
    """alice (admin) with two devices: returns (phone, laptop) token pairs."""
    body = {"handle": "alice", "display_name": "Alice", "password": PW}
    await client.post(f"{AUTH}/register", json=body)
    return await _pair(client, "alice"), await _pair(client, "alice")


# ---------------------------------------------------------------- REST


async def test_change_signs_other_devices_out_immediately(client: httpx.AsyncClient) -> None:
    phone, laptop = await _setup(client)
    assert (await client.get(f"{AUTH}/me", headers=_h(phone["access_token"]))).status_code == 200

    r = await client.post(
        f"{AUTH}/password",
        json={"current_password": PW, "new_password": "brand-new-pass"},
        headers=_h(laptop["access_token"]),
    )
    assert r.status_code == 200
    # The phone's access token is still inside its 15 minutes, and refused now.
    me = await client.get(f"{AUTH}/me", headers=_h(phone["access_token"]))
    assert me.status_code == 401
    assert me.json()["error"]["code"] == "auth.invalid_token"
    # The laptop's OLD access token too; the pair returned by the change works.
    assert (await client.get(f"{AUTH}/me", headers=_h(laptop["access_token"]))).status_code == 401
    assert (await client.get(f"{AUTH}/me", headers=_h(r.json()["access_token"]))).status_code == 200


async def test_unchecked_keeps_other_devices_signed_in(client: httpx.AsyncClient) -> None:
    phone, laptop = await _setup(client)
    r = await client.post(
        f"{AUTH}/password",
        json={
            "current_password": PW,
            "new_password": "brand-new-pass",
            "sign_out_other_devices": False,
        },
        headers=_h(laptop["access_token"]),
    )
    assert r.status_code == 200
    assert (await client.get(f"{AUTH}/me", headers=_h(phone["access_token"]))).status_code == 200
    refreshed = await client.post(f"{AUTH}/refresh", json={"refresh_token": phone["refresh_token"]})
    assert refreshed.status_code == 200
    # The password itself did change.
    assert (
        await client.post(f"{AUTH}/login", json={"handle": "alice", "password": PW})
    ).status_code == 401


async def test_admin_reset_cuts_the_target_off_immediately(client: httpx.AsyncClient) -> None:
    admin, _ = await _setup(client)
    body = {"handle": "bob", "display_name": "Bob", "password": PW}
    await client.post(f"{AUTH}/register", json=body, headers=_h(admin["access_token"]))
    bob = await _pair(client, "bob")
    bob_id = (await client.get(f"{AUTH}/me", headers=_h(bob["access_token"]))).json()["id"]

    r = await client.post(
        f"/api/v1/users/{bob_id}/password",
        json={"admin_password": PW, "new_password": "temporary-pass"},
        headers=_h(admin["access_token"]),
    )
    assert r.status_code == 204
    assert (await client.get(f"{AUTH}/me", headers=_h(bob["access_token"]))).status_code == 401
    # The admin is untouched.
    assert (await client.get(f"{AUTH}/me", headers=_h(admin["access_token"]))).status_code == 200
    # Bob signs in again with the new password and is fine.
    fresh = await _pair(client, "bob", "temporary-pass")
    assert (await client.get(f"{AUTH}/me", headers=_h(fresh["access_token"]))).status_code == 200


# ---------------------------------------------------------------- units


def test_cutoff_is_millisecond_precise() -> None:
    """A token minted earlier in the SAME second as the change is revoked (iat
    alone, being whole seconds, would have spared it)."""
    cutoff = datetime(2026, 9, 25, 12, 0, 0, 500_000, tzinfo=UTC)
    user = User(handle="a", display_name="a", sessions_valid_after=cutoff)
    ms = int(cutoff.timestamp() * 1000)
    assert user.session_revoked(ms - 1)
    assert not user.session_revoked(ms)
    assert not User(handle="b", display_name="b").session_revoked(0)  # never revoked


def test_tokens_without_iat_ms_round_down() -> None:
    """Tokens from before iat_ms existed are treated as older, never younger."""
    assert issued_at_ms({"iat": 1_000, "iat_ms": 1_000_999}) == 1_000_999
    assert issued_at_ms({"iat": 1_000}) == 1_000_000
    assert issued_at_ms({"iat": 1_000, "iat_ms": True}) == 1_000_000  # not an int claim


# ---------------------------------------------------------------- WebSocket


def test_open_sockets_of_other_devices_are_closed(sync_client: TestClient) -> None:
    http = sync_client
    body = {"handle": "alice", "display_name": "Alice", "password": PW}
    assert http.post(f"{AUTH}/register", json=body).status_code == 201
    phone = http.post(f"{AUTH}/login", json={"handle": "alice", "password": PW}).json()
    laptop = http.post(f"{AUTH}/login", json={"handle": "alice", "password": PW}).json()

    with http.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": phone["access_token"]}})
        assert ws.receive_json()["type"] == "ready"
        r = http.post(
            f"{AUTH}/password",
            json={"current_password": PW, "new_password": "brand-new-pass"},
            headers=_h(laptop["access_token"]),
        )
        assert r.status_code == 200
        # A probe the server would answer (unknown type → error frame) if the socket
        # were still open: the test then fails on a reply instead of hanging on a
        # close that never comes.
        ws.send_json({"type": "probe.unknown", "id": "p1"})
        with pytest.raises(WebSocketDisconnect) as closed:
            ws.receive_json()
        assert closed.value.code == 1008
        assert closed.value.reason == "session_revoked"

    # Reconnecting with the revoked token is refused the same way, and it is not a
    # failed login: the owner's other devices share this IP.
    for _ in range(10):
        with http.websocket_connect("/ws") as ws:
            ws.send_json({"type": "auth", "data": {"access_token": phone["access_token"]}})
            with pytest.raises(WebSocketDisconnect) as again:
                ws.receive_json()
            assert again.value.reason == "session_revoked"
    ok = http.post(f"{AUTH}/login", json={"handle": "alice", "password": "brand-new-pass"})
    assert ok.status_code == 200  # 10 revoked reconnects did not push the IP into backoff


def test_unchecked_leaves_other_sockets_open(sync_client: TestClient) -> None:
    http = sync_client
    body = {"handle": "alice", "display_name": "Alice", "password": PW}
    assert http.post(f"{AUTH}/register", json=body).status_code == 201
    phone = http.post(f"{AUTH}/login", json={"handle": "alice", "password": PW}).json()
    laptop = http.post(f"{AUTH}/login", json={"handle": "alice", "password": PW}).json()

    with http.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": phone["access_token"]}})
        assert ws.receive_json()["type"] == "ready"
        r = http.post(
            f"{AUTH}/password",
            json={
                "current_password": PW,
                "new_password": "brand-new-pass",
                "sign_out_other_devices": False,
            },
            headers=_h(laptop["access_token"]),
        )
        assert r.status_code == 200
        # Still usable: a re-auth with the same (unrevoked) token is answered.
        ws.send_json({"type": "auth", "id": "r1", "data": {"access_token": phone["access_token"]}})
        assert ws.receive_json()["type"] == "ready"
