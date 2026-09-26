"""Offering channel ownership (owner decision, 2026-09-26).

An owner or an admin offers; the member accepts or declines when they next open the
channel, however long that takes (a queue: no expiry); accepting adds an owner (the one
who offered stays one); one pending offer per member; an offer ends if either side leaves.
The offer rides on the channel object, so /sync carries it to a member who was offline.
"""

from __future__ import annotations

import httpx
from fastapi.testclient import TestClient

from tests.test_members import AUTH, PW, _h, _remove, _team, _user

CH = "/api/v1/channels"


def _offer(client: httpx.AsyncClient, ch: str, handle: str, h: dict[str, str]):  # type: ignore[no-untyped-def]
    return client.post(f"{CH}/{ch}/owner-offers", json={"handle": handle}, headers=h)


async def _channel(client: httpx.AsyncClient, ch: str, h: dict[str, str]) -> dict:
    listed = (await client.get(CH, headers=h)).json()
    (channel,) = [c for c in listed if c["id"] == ch]
    return dict(channel)


def _roles(channel: dict) -> dict[str, str]:
    return {m["handle"]: m["role"] for m in channel["members"]}


async def test_an_offer_waits_for_an_offline_member_then_is_accepted(
    client: httpx.AsyncClient,
) -> None:
    t = await _team(client)
    hb, hc, ch = t["hb"], t["hc"], t["ch"]
    # carol was last in sync here; then she goes offline
    cursor = (await client.get("/api/v1/sync", params={"since": "0"}, headers=hc)).json()["next"]

    r = await _offer(client, ch, "carol", hb)
    assert r.status_code == 201, r.text
    assert r.json()["owner_offers"] == [
        {
            "user_id": t["c"],
            "offered_by": t["b"],
            "created_at": r.json()["owner_offers"][0]["created_at"],
        }
    ]
    # offering again keeps the one offer (a queue of one per member)
    again = await _offer(client, ch, "carol", hb)
    assert again.status_code == 200 and len(again.json()["owner_offers"]) == 1

    # carol comes back: /sync brings the channel with her offer (the queue)
    synced = (await client.get("/api/v1/sync", params={"since": cursor}, headers=hc)).json()
    (channel,) = [c for c in synced["channels"] if c["id"] == ch]
    assert [o["user_id"] for o in channel["owner_offers"]] == [t["c"]]

    cursor_b = (await client.get("/api/v1/sync", params={"since": "0"}, headers=hb)).json()["next"]
    accepted = await client.post(f"{CH}/{ch}/owner-offers/accept", headers=hc)
    assert accepted.status_code == 200, accepted.text
    assert accepted.json()["owner_offers"] == []
    # an owner is added: the one who offered stays one
    assert _roles(accepted.json()) == {"bob": "owner", "carol": "owner", "dave": "member"}
    # and bob's /sync sees the new role (a role change re-stamps the channel)
    synced = (await client.get("/api/v1/sync", params={"since": cursor_b}, headers=hb)).json()
    (channel,) = [c for c in synced["channels"] if c["id"] == ch]
    assert _roles(channel)["carol"] == "owner" and channel["owner_offers"] == []
    # with two owners, bob can now leave
    assert (await _remove(client, ch, t["b"], hb)).status_code == 204


async def test_declining_changes_nothing_but_the_offer(client: httpx.AsyncClient) -> None:
    t = await _team(client)
    hb, hc, ch = t["hb"], t["hc"], t["ch"]
    assert (await _offer(client, ch, "carol", hb)).status_code == 201
    assert (await client.post(f"{CH}/{ch}/owner-offers/decline", headers=hc)).status_code == 204
    channel = await _channel(client, ch, hb)
    assert channel["owner_offers"] == [] and _roles(channel)["carol"] == "member"
    # nothing left to answer
    r = await client.post(f"{CH}/{ch}/owner-offers/accept", headers=hc)
    assert r.status_code == 404 and r.json()["error"]["code"] == "offer.not_found"


async def test_withdrawing_an_offer(client: httpx.AsyncClient) -> None:
    t = await _team(client)
    hb, hc, ch = t["hb"], t["hc"], t["ch"]
    assert (await _offer(client, ch, "carol", hb)).status_code == 201
    # a member can't withdraw someone else's offer
    r = await client.delete(f"{CH}/{ch}/owner-offers/{t['c']}", headers=t["hd"])
    assert r.status_code == 403
    assert (await client.delete(f"{CH}/{ch}/owner-offers/{t['c']}", headers=hb)).status_code == 204
    r = await client.post(f"{CH}/{ch}/owner-offers/accept", headers=hc)
    assert r.status_code == 404  # withdrawn before she answered


async def test_who_may_offer_and_to_whom(client: httpx.AsyncClient) -> None:
    t = await _team(client)
    ha, hb, hc, ch = t["ha"], t["hb"], t["hc"], t["ch"]
    r = await _offer(client, ch, "dave", hc)  # a member can't offer
    assert r.status_code == 403
    he, _ = await _user(client, "eve", ha)
    r = await _offer(client, ch, "dave", he)  # an outsider learns nothing
    assert r.status_code == 404
    r = await _offer(client, ch, "eve", hb)  # not a member
    assert r.status_code == 422 and r.json()["error"]["code"] == "channel.not_member"
    r = await _offer(client, ch, "bob", hb)  # already an owner
    assert r.status_code == 409 and r.json()["error"]["code"] == "channel.already_owner"
    assert (await _offer(client, ch, "dave", ha)).status_code == 201  # an admin may


async def test_a_dm_has_no_owners(client: httpx.AsyncClient) -> None:
    ha, _ = await _user(client, "alice")
    await _user(client, "bob", ha)
    dm = (await client.post(CH, json={"kind": "dm", "member": "bob"}, headers=ha)).json()["id"]
    r = await _offer(client, dm, "bob", ha)
    assert r.status_code == 422 and r.json()["error"]["code"] == "channel.dm"


async def test_an_offer_ends_when_either_side_leaves(client: httpx.AsyncClient) -> None:
    t = await _team(client)
    hb, hc, hd, ch = t["hb"], t["hc"], t["hd"], t["ch"]
    # the recipient leaves
    assert (await _offer(client, ch, "dave", hb)).status_code == 201
    assert (await _remove(client, ch, t["d"], hd)).status_code == 204
    assert (await _channel(client, ch, hb))["owner_offers"] == []
    # the one who offered leaves (carol becomes an owner first, so bob may leave)
    assert (await _offer(client, ch, "carol", hb)).status_code == 201
    assert (await client.post(f"{CH}/{ch}/owner-offers/accept", headers=hc)).status_code == 200
    body = {"handle": "dave"}
    assert (await client.post(f"{CH}/{ch}/members", json=body, headers=hc)).status_code == 204
    assert (await _offer(client, ch, "dave", hb)).status_code == 201
    assert (await _remove(client, ch, t["b"], hb)).status_code == 204
    assert (await _channel(client, ch, hc))["owner_offers"] == []


def test_the_recipient_hears_the_offer_live(sync_client: TestClient) -> None:
    http = sync_client

    def user(handle: str, admin: dict[str, str] | None = None) -> tuple[dict[str, str], str]:
        body = {"handle": handle, "display_name": handle.title(), "password": PW}
        http.post(f"{AUTH}/register", json=body, headers=admin or {})
        tok = http.post(f"{AUTH}/login", json={"handle": handle, "password": PW}).json()
        h = _h(tok["access_token"])
        return h, str(http.get(f"{AUTH}/me", headers=h).json()["id"])

    ha, _a = user("alice")
    hb, b = user("bob", ha)
    ch = http.post(CH, json={"kind": "channel", "name": "g"}, headers=ha).json()["id"]
    http.post(f"{CH}/{ch}/members", json={"handle": "bob"}, headers=ha)
    with http.websocket_connect("/ws") as wb:
        wb.send_json({"type": "auth", "data": {"access_token": hb["Authorization"][7:]}})
        assert wb.receive_json()["type"] == "ready"
        assert (
            http.post(f"{CH}/{ch}/owner-offers", json={"handle": "bob"}, headers=ha).status_code
            == 201
        )
        # a probe bounds the read: sent after the offer, so a missing update fails
        http.post(f"{CH}/{ch}/messages", json={"body": "probe"}, headers=ha)
        updates = []
        while True:
            f = wb.receive_json()
            if f["type"] == "channel.update":
                updates.append(f)
            if f["type"] == "message.new" and f["data"]["body"] == "probe":
                break
    assert [o["user_id"] for o in updates[-1]["data"]["owner_offers"]] == [b]


async def test_only_the_recipient_can_answer_an_offer(client: httpx.AsyncClient) -> None:
    """The property everything hinges on: an offer to carol is carol's alone. Another
    member, and the one who offered, get 404 on accept and decline; the offer stays
    and nobody's role changes. (A lookup by channel instead of by caller passed every
    other test: the auth review's mutant.)"""
    t = await _team(client)
    hb, hc, hd, ch = t["hb"], t["hc"], t["hd"], t["ch"]
    assert (await _offer(client, ch, "carol", hb)).status_code == 201
    for who in (hd, hb):  # another member; the offerer (an owner already)
        for answer in ("accept", "decline"):
            r = await client.post(f"{CH}/{ch}/owner-offers/{answer}", headers=who)
            assert r.status_code == 404 and r.json()["error"]["code"] == "offer.not_found"
    channel = await _channel(client, ch, hc)
    assert [o["user_id"] for o in channel["owner_offers"]] == [t["c"]]
    assert _roles(channel) == {"bob": "owner", "carol": "member", "dave": "member"}


async def test_an_outsider_learns_nothing_on_any_offer_route(client: httpx.AsyncClient) -> None:
    t = await _team(client)
    ch = t["ch"]
    assert (await _offer(client, ch, "carol", t["hb"])).status_code == 201
    he, _ = await _user(client, "eve", t["ha"])
    for r in (
        await client.post(f"{CH}/{ch}/owner-offers/accept", headers=he),
        await client.post(f"{CH}/{ch}/owner-offers/decline", headers=he),
        await client.delete(f"{CH}/{ch}/owner-offers/{t['c']}", headers=he),
    ):
        assert r.status_code == 404 and r.json()["error"]["code"] == "not_found"


async def test_a_public_listing_shows_no_offers(client: httpx.AsyncClient) -> None:
    ha, _ = await _user(client, "alice")
    await _user(client, "bob", ha)
    he, _ = await _user(client, "eve", ha)
    ch = (
        await client.post(CH, json={"kind": "channel", "name": "town", "public": True}, headers=ha)
    ).json()["id"]
    await client.post(f"{CH}/{ch}/members", json={"handle": "bob"}, headers=ha)
    assert (await _offer(client, ch, "bob", ha)).status_code == 201
    listed = (await client.get(f"{CH}/public", headers=he)).json()
    (town,) = [c for c in listed if c["id"] == ch]
    assert town["owner_offers"] == []  # members see it; someone browsing doesn't
