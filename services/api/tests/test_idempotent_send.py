"""Outbox idempotency (sync spec §4): a message sent with a client_id is stored once."""

from __future__ import annotations

import uuid

import httpx
from fastapi.testclient import TestClient

AUTH = "/api/v1/auth"
PW = "supersecret"


async def _setup(client: httpx.AsyncClient) -> tuple[dict[str, str], dict[str, str], str]:
    """alice (admin) and bob, both in a channel; returns their bearer headers + channel id."""
    await client.post(
        f"{AUTH}/register", json={"handle": "alice", "display_name": "A", "password": PW}
    )
    a = (await client.post(f"{AUTH}/login", json={"handle": "alice", "password": PW})).json()
    ha = {"Authorization": f"Bearer {a['access_token']}"}
    await client.post(
        f"{AUTH}/register", json={"handle": "bob", "display_name": "B", "password": PW}, headers=ha
    )
    b = (await client.post(f"{AUTH}/login", json={"handle": "bob", "password": PW})).json()
    hb = {"Authorization": f"Bearer {b['access_token']}"}
    ch = (
        await client.post(
            "/api/v1/channels", json={"kind": "channel", "name": "general"}, headers=ha
        )
    ).json()
    await client.post(f"/api/v1/channels/{ch['id']}/members", json={"handle": "bob"}, headers=ha)
    return ha, hb, ch["id"]


async def _history(client: httpx.AsyncClient, h: dict[str, str], ch: str) -> list[dict]:
    return list((await client.get(f"/api/v1/channels/{ch}/messages", headers=h)).json())


async def test_resend_returns_the_stored_message(client: httpx.AsyncClient) -> None:
    ha, _hb, ch = await _setup(client)
    cid = str(uuid.uuid4())
    first = await client.post(
        f"/api/v1/channels/{ch}/messages", json={"body": "hi", "client_id": cid}, headers=ha
    )
    assert first.status_code == 201 and first.json()["client_id"] == cid
    again = await client.post(
        f"/api/v1/channels/{ch}/messages",
        json={"body": "hi (edited offline)", "client_id": cid},
        headers=ha,
    )
    # Same outbox entry: the stored message wins, unchanged; 200, not 201, never 409.
    assert again.status_code == 200
    assert again.json()["id"] == first.json()["id"] and again.json()["body"] == "hi"
    assert [m["body"] for m in await _history(client, ha, ch)] == ["hi"]


async def test_client_id_is_scoped_per_author(client: httpx.AsyncClient) -> None:
    ha, hb, ch = await _setup(client)
    cid = str(uuid.uuid4())
    a = await client.post(
        f"/api/v1/channels/{ch}/messages", json={"body": "from a", "client_id": cid}, headers=ha
    )
    b = await client.post(
        f"/api/v1/channels/{ch}/messages", json={"body": "from b", "client_id": cid}, headers=hb
    )
    assert a.status_code == 201 and b.status_code == 201
    assert a.json()["id"] != b.json()["id"]


async def test_without_client_id_every_send_is_new(client: httpx.AsyncClient) -> None:
    ha, _hb, ch = await _setup(client)
    for _ in range(2):
        r = await client.post(f"/api/v1/channels/{ch}/messages", json={"body": "same"}, headers=ha)
        assert r.status_code == 201 and r.json()["client_id"] is None
    assert len(await _history(client, ha, ch)) == 2


def test_live_event_echoes_client_id(sync_client: TestClient) -> None:
    http = sync_client
    http.post(f"{AUTH}/register", json={"handle": "alice", "display_name": "A", "password": PW})
    a = http.post(f"{AUTH}/login", json={"handle": "alice", "password": PW}).json()
    ha = {"Authorization": f"Bearer {a['access_token']}"}
    ch = http.post(
        "/api/v1/channels", json={"kind": "channel", "name": "general"}, headers=ha
    ).json()
    cid = str(uuid.uuid4())
    with http.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": a["access_token"]}})
        assert ws.receive_json()["type"] == "ready"
        http.post(
            f"/api/v1/channels/{ch['id']}/messages",
            json={"body": "hi", "client_id": cid},
            headers=ha,
        )
        while True:
            ev = ws.receive_json()
            if ev["type"] == "message.new":
                break
        assert ev["data"]["client_id"] == cid
