"""Channel / DM / message REST tests."""

from __future__ import annotations

import httpx

API = "/api/v1"


async def _register(client: httpx.AsyncClient, handle: str, **kw: object) -> httpx.Response:
    body = {"handle": handle, "display_name": handle.title(), "password": "supersecret"}
    return await client.post(f"{API}/auth/register", json=body, **kw)  # type: ignore[arg-type]


async def _token(client: httpx.AsyncClient, handle: str) -> str:
    resp = await client.post(
        f"{API}/auth/login", json={"handle": handle, "password": "supersecret"}
    )
    return str(resp.json()["access_token"])


def _auth(token: str) -> dict[str, str]:
    return {"Authorization": f"Bearer {token}"}


async def _two_users(client: httpx.AsyncClient) -> tuple[str, str]:
    """Register alice (admin) + bob (member); return their access tokens."""
    await _register(client, "alice")  # first user → admin
    alice = await _token(client, "alice")
    await _register(client, "bob", headers=_auth(alice))
    bob = await _token(client, "bob")
    return alice, bob


async def test_admin_creates_channel_member_cannot(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)

    created = await client.post(
        f"{API}/channels", json={"kind": "channel", "name": "general"}, headers=_auth(alice)
    )
    assert created.status_code == 201
    assert created.json()["kind"] == "channel"
    assert created.json()["name"] == "general"

    forbidden = await client.post(
        f"{API}/channels", json={"kind": "channel", "name": "secret"}, headers=_auth(bob)
    )
    assert forbidden.status_code == 403
    assert forbidden.json()["error"]["code"] == "authz.forbidden"


async def test_dm_is_find_or_create(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)
    first = await client.post(
        f"{API}/channels", json={"kind": "dm", "member": "bob"}, headers=_auth(alice)
    )
    assert first.status_code == 201
    assert {m["handle"] for m in first.json()["members"]} == {"alice", "bob"}

    # opening it again (from either side) returns the same channel, not a new one
    again = await client.post(
        f"{API}/channels", json={"kind": "dm", "member": "alice"}, headers=_auth(bob)
    )
    assert again.json()["id"] == first.json()["id"]


async def test_send_and_history_and_membership(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)
    dm = (
        await client.post(
            f"{API}/channels", json={"kind": "dm", "member": "bob"}, headers=_auth(alice)
        )
    ).json()
    cid = dm["id"]

    for text in ("hello", "how are you?"):
        sent = await client.post(
            f"{API}/channels/{cid}/messages", json={"body": text}, headers=_auth(alice)
        )
        assert sent.status_code == 201
        assert sent.json()["author_handle"] == "alice"

    # both members see the same history, oldest→newest
    hist = await client.get(f"{API}/channels/{cid}/messages", headers=_auth(bob))
    assert [m["body"] for m in hist.json()] == ["hello", "how are you?"]

    # both members see the DM in their channel list
    alice_list = await client.get(f"{API}/channels", headers=_auth(alice))
    assert cid in {c["id"] for c in alice_list.json()}


async def test_non_member_cannot_read_or_post(client: httpx.AsyncClient) -> None:
    alice, _bob = await _two_users(client)
    await _register(client, "carol", headers=_auth(alice))
    carol = await _token(client, "carol")

    chan = (
        await client.post(
            f"{API}/channels", json={"kind": "channel", "name": "general"}, headers=_auth(alice)
        )
    ).json()
    cid = chan["id"]

    # carol is not a member → 404 (existence not leaked), for both read and write
    assert (
        await client.get(f"{API}/channels/{cid}/messages", headers=_auth(carol))
    ).status_code == 404
    assert (
        await client.post(
            f"{API}/channels/{cid}/messages", json={"body": "hi"}, headers=_auth(carol)
        )
    ).status_code == 404


async def test_admin_adds_member_then_they_can_post(client: httpx.AsyncClient) -> None:
    alice, _bob = await _two_users(client)
    await _register(client, "carol", headers=_auth(alice))
    carol = await _token(client, "carol")
    chan = (
        await client.post(
            f"{API}/channels", json={"kind": "channel", "name": "general"}, headers=_auth(alice)
        )
    ).json()
    cid = chan["id"]

    added = await client.post(
        f"{API}/channels/{cid}/members", json={"handle": "carol"}, headers=_auth(alice)
    )
    assert added.status_code == 204

    posted = await client.post(
        f"{API}/channels/{cid}/messages", json={"body": "hi all"}, headers=_auth(carol)
    )
    assert posted.status_code == 201


async def test_history_pagination_before(client: httpx.AsyncClient) -> None:
    alice, _bob = await _two_users(client)
    chan = (
        await client.post(
            f"{API}/channels", json={"kind": "channel", "name": "general"}, headers=_auth(alice)
        )
    ).json()
    cid = chan["id"]
    ids = []
    for i in range(5):
        r = await client.post(
            f"{API}/channels/{cid}/messages", json={"body": f"m{i}"}, headers=_auth(alice)
        )
        ids.append(r.json()["id"])

    page = await client.get(
        f"{API}/channels/{cid}/messages", params={"limit": 2}, headers=_auth(alice)
    )
    assert [m["body"] for m in page.json()] == ["m3", "m4"]

    older = await client.get(
        f"{API}/channels/{cid}/messages",
        params={"before": ids[3], "limit": 2},
        headers=_auth(alice),
    )
    assert [m["body"] for m in older.json()] == ["m1", "m2"]
