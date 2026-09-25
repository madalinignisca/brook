"""Sign out everywhere: a password change (checkbox, default on) or an admin reset
revokes every other session at once, on REST and on open WebSockets, instead of
when the 15-minute access token expires."""

from __future__ import annotations

import uuid
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
    assert r.json()["other_devices_signed_out"] is True
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
    assert r.json()["other_devices_signed_out"] is False
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


async def test_register_refuses_a_revoked_admin_token(client: httpx.AsyncClient) -> None:
    """A stolen admin token cut off by a password change can't still create accounts.

    register resolves its optional caller separately from get_current_user; it once
    skipped the revocation check (auth review of #45)."""
    stolen, laptop = await _setup(client)
    r = await client.post(
        f"{AUTH}/password",
        json={"current_password": PW, "new_password": "brand-new-pass"},
        headers=_h(laptop["access_token"]),
    )
    assert r.status_code == 200
    body = {"handle": "mallory", "display_name": "Mallory", "password": "mallory-pass"}
    created = await client.post(f"{AUTH}/register", json=body, headers=_h(stolen["access_token"]))
    assert created.status_code == 403
    login = await client.post(
        f"{AUTH}/login", json={"handle": "mallory", "password": "mallory-pass"}
    )
    assert login.status_code == 401  # no account was created


def test_reauth_with_a_revoked_token_is_session_revoked(sync_client: TestClient) -> None:
    """The on-socket `auth` frame path: a revoked token closes `session_revoked`
    (not `auth_failed`, which would count against the owner's IP)."""
    http = sync_client
    body = {"handle": "alice", "display_name": "Alice", "password": PW}
    assert http.post(f"{AUTH}/register", json=body).status_code == 201
    old = http.post(f"{AUTH}/login", json={"handle": "alice", "password": PW}).json()
    r = http.post(
        f"{AUTH}/password",
        json={"current_password": PW, "new_password": "brand-new-pass"},
        headers=_h(old["access_token"]),
    )
    fresh = r.json()
    with http.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": fresh["access_token"]}})
        assert ws.receive_json()["type"] == "ready"
        ws.send_json({"type": "auth", "id": "r1", "data": {"access_token": old["access_token"]}})
        with pytest.raises(WebSocketDisconnect) as closed:
            ws.receive_json()
        assert closed.value.reason == "session_revoked"


def test_a_socket_registering_after_the_sweep_is_closed(
    sync_client: TestClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The registration race: a token checked just before a revocation committed
    must not survive by registering after revoke_sessions() swept the live set."""
    from app.routers import ws as ws_module

    http = sync_client
    body = {"handle": "alice", "display_name": "Alice", "password": PW}
    assert http.post(f"{AUTH}/register", json=body).status_code == 201
    pair = http.post(f"{AUTH}/login", json={"handle": "alice", "password": PW}).json()
    user_id = http.get(f"{AUTH}/me", headers=_h(pair["access_token"])).json()["id"]
    # The sweep already ran (cutoff recorded) but the DB read that authenticated
    # this socket happened before the commit: model it by recording the cutoff only.
    monkeypatch.setitem(ws_module._cutoffs, uuid.UUID(user_id), 2**62)
    with http.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": pair["access_token"]}})
        with pytest.raises(WebSocketDisconnect) as closed:
            ws.receive_json()
        assert closed.value.reason == "session_revoked"
