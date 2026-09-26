"""Leaving or removing a channel member, and editing your own profile.

Owner decision, 2026-09-26: anyone may leave; a channel owner or an admin removes
others; only an admin removes an owner; the last owner stays; a DM can't be left. A
profile has a display name (1..64) and a status line (0..100); the handle is fixed.
"""

from __future__ import annotations

import uuid

import httpx
from fastapi.testclient import TestClient
from sqlalchemy import delete, update

from app import db
from app.models import Membership

AUTH = "/api/v1/auth"
PW = "supersecret"


def _h(token: str) -> dict[str, str]:
    return {"Authorization": f"Bearer {token}"}


async def _user(
    client: httpx.AsyncClient, handle: str, admin: dict[str, str] | None = None
) -> tuple[dict[str, str], str]:
    body = {"handle": handle, "display_name": handle.title(), "password": PW}
    await client.post(f"{AUTH}/register", json=body, headers=admin or {})
    r = await client.post(f"{AUTH}/login", json={"handle": handle, "password": PW})
    h = _h(r.json()["access_token"])
    me = await client.get(f"{AUTH}/me", headers=h)
    return h, str(me.json()["id"])


async def _team(client: httpx.AsyncClient) -> dict[str, object]:
    """alice (the first user: admin) makes "team" and steps out; bob owns it; carol and
    dave are members. Only admins create channels, and there's no promote route yet, so
    the owner is set directly, as an ownership hand-over would."""
    ha, a = await _user(client, "alice")
    hb, b = await _user(client, "bob", ha)
    hc, c = await _user(client, "carol", ha)
    hd, d = await _user(client, "dave", ha)
    ch = (
        await client.post("/api/v1/channels", json={"kind": "channel", "name": "team"}, headers=ha)
    ).json()["id"]
    for handle in ("bob", "carol", "dave"):
        r = await client.post(f"/api/v1/channels/{ch}/members", json={"handle": handle}, headers=ha)
        assert r.status_code == 204
    async with db.get_sessionmaker()() as s:
        cid = uuid.UUID(ch)
        await s.execute(
            update(Membership)
            .where(Membership.channel_id == cid, Membership.user_id == uuid.UUID(b))
            .values(role="owner")
        )
        await s.execute(
            delete(Membership).where(
                Membership.channel_id == cid, Membership.user_id == uuid.UUID(a)
            )
        )
        await s.commit()
    return {"ha": ha, "hb": hb, "hc": hc, "hd": hd, "a": a, "b": b, "c": c, "d": d, "ch": ch}


def _remove(client: httpx.AsyncClient, ch: str, uid: str, h: dict[str, str]):  # type: ignore[no-untyped-def]
    return client.delete(f"/api/v1/channels/{ch}/members/{uid}", headers=h)


async def _members(client: httpx.AsyncClient, ch: str, h: dict[str, str]) -> set[str]:
    r = await client.get("/api/v1/channels", headers=h)
    (channel,) = [c for c in r.json() if c["id"] == ch]
    return {m["handle"] for m in channel["members"]}


# ---- leave / remove ------------------------------------------------------------------


async def test_a_member_leaves_and_everyone_syncs_it(client: httpx.AsyncClient) -> None:
    t = await _team(client)
    hb, hc, ch = t["hb"], t["hc"], t["ch"]
    cursor_b = (await client.get("/api/v1/sync", params={"since": "0"}, headers=hb)).json()["next"]
    cursor_c = (await client.get("/api/v1/sync", params={"since": "0"}, headers=hc)).json()["next"]

    assert (await _remove(client, ch, t["c"], hc)).status_code == 204  # carol leaves

    assert await _members(client, ch, hb) == {"bob", "dave"}
    left = await client.get("/api/v1/sync", params={"since": cursor_b}, headers=hb)
    assert {(x["channel_id"], x["user_id"]) for x in left.json()["left_members"]} == {(ch, t["c"])}
    gone = await client.get("/api/v1/sync", params={"since": cursor_c}, headers=hc)
    assert [x["channel_id"] for x in gone.json()["removed_channels"]] == [ch]
    # and she can't read it any more (404: its existence isn't revealed)
    assert (await client.get(f"/api/v1/channels/{ch}/messages", headers=hc)).status_code == 404


async def test_who_may_remove_whom(client: httpx.AsyncClient) -> None:
    t = await _team(client)
    ha, hb, hc, ch = t["ha"], t["hb"], t["hc"], t["ch"]
    # a member can't remove another member
    r = await _remove(client, ch, t["d"], hc)
    assert r.status_code == 403 and r.json()["error"]["code"] == "authz.forbidden"
    # the owner can
    assert (await _remove(client, ch, t["d"], hb)).status_code == 204
    # an admin who isn't in the channel can
    assert (await _remove(client, ch, t["c"], ha)).status_code == 204
    assert await _members(client, ch, hb) == {"bob"}


async def test_an_outsider_learns_nothing(client: httpx.AsyncClient) -> None:
    t = await _team(client)
    he, _e = await _user(client, "eve", t["ha"])
    r = await _remove(client, t["ch"], t["c"], he)
    assert r.status_code == 404 and r.json()["error"]["code"] == "not_found"
    r = await _remove(client, t["ch"], str(uuid.uuid4()), t["hb"])  # not a member
    assert r.status_code == 404


async def test_the_last_owner_stays_and_only_an_admin_removes_an_owner(
    client: httpx.AsyncClient,
) -> None:
    t = await _team(client)
    ha, hb, hc, ch = t["ha"], t["hb"], t["hc"], t["ch"]
    r = await _remove(client, ch, t["b"], hb)  # bob, the only owner, leaves
    assert r.status_code == 409 and r.json()["error"]["code"] == "channel.last_owner"
    r = await _remove(client, ch, t["b"], ha)  # an admin can't strand it either
    assert r.status_code == 409

    # With a second owner: an owner can't remove another owner, an admin can.
    async with db.get_sessionmaker()() as s:
        await s.execute(
            update(Membership)
            .where(Membership.channel_id == uuid.UUID(ch), Membership.user_id == uuid.UUID(t["c"]))
            .values(role="owner")
        )
        await s.commit()
    r = await _remove(client, ch, t["b"], hc)
    assert r.status_code == 403
    assert (await _remove(client, ch, t["b"], ha)).status_code == 204
    # and now carol is the last owner again
    assert (await _remove(client, ch, t["c"], hc)).status_code == 409


async def test_a_dm_cant_be_left(client: httpx.AsyncClient) -> None:
    ha, a = await _user(client, "alice")
    await _user(client, "bob", ha)
    dm = (
        await client.post("/api/v1/channels", json={"kind": "dm", "member": "bob"}, headers=ha)
    ).json()["id"]
    r = await _remove(client, dm, a, ha)
    assert r.status_code == 422 and r.json()["error"]["code"] == "channel.dm"


def test_removal_is_live_for_both_sides(sync_client: TestClient) -> None:
    """The removed user hears channel.delete (core fences the channel on it); the others
    hear channel.update without them."""
    http = sync_client

    def user(handle: str, admin: dict[str, str] | None = None) -> tuple[dict[str, str], str]:
        body = {"handle": handle, "display_name": handle.title(), "password": PW}
        http.post(f"{AUTH}/register", json=body, headers=admin or {})
        tok = http.post(f"{AUTH}/login", json={"handle": handle, "password": PW}).json()
        h = _h(tok["access_token"])
        return h, str(http.get(f"{AUTH}/me", headers=h).json()["id"])

    ha, _a = user("alice")
    hb, b = user("bob", ha)
    ch = http.post("/api/v1/channels", json={"kind": "channel", "name": "g"}, headers=ha).json()
    http.post(f"/api/v1/channels/{ch['id']}/members", json={"handle": "bob"}, headers=ha)
    token = {"a": ha["Authorization"][7:], "b": hb["Authorization"][7:]}

    def frames_until(ws, kind: str) -> dict:  # type: ignore[no-untyped-def]
        while True:
            f = ws.receive_json()
            if f["type"] == kind:
                return dict(f)

    with http.websocket_connect("/ws") as wa, http.websocket_connect("/ws") as wb:
        wa.send_json({"type": "auth", "data": {"access_token": token["a"]}})
        wb.send_json({"type": "auth", "data": {"access_token": token["b"]}})
        assert wa.receive_json()["type"] == "ready" and wb.receive_json()["type"] == "ready"
        r = http.delete(f"/api/v1/channels/{ch['id']}/members/{b}", headers=ha)
        assert r.status_code == 204
        # A probe bounds bob's read: a DM message sent after the removal arrives after
        # anything the removal sent him, so a missing channel.delete fails, never hangs.
        dm = http.post("/api/v1/channels", json={"kind": "dm", "member": "bob"}, headers=ha).json()
        http.post(f"/api/v1/channels/{dm['id']}/messages", json={"body": "probe"}, headers=ha)
        seen = []
        while True:
            f = wb.receive_json()
            seen.append(f)
            if f["type"] == "message.new" and f["data"]["body"] == "probe":
                break
        gone = [f for f in seen if f["type"] == "channel.delete"]
        assert len(gone) == 1, [f["type"] for f in seen]
        assert gone[0]["data"]["id"] == ch["id"] and gone[0]["data"]["seq"] > 0
        upd = frames_until(wa, "channel.update")
        assert "bob" not in {m["handle"] for m in upd["data"]["members"]}


# ---- profile -------------------------------------------------------------------------


async def test_edit_your_profile_and_others_sync_it(client: httpx.AsyncClient) -> None:
    t = await _team(client)
    hb, hc = t["hb"], t["hc"]
    cursor = (await client.get("/api/v1/sync", params={"since": "0"}, headers=hb)).json()["next"]

    r = await client.patch(
        f"{AUTH}/me", json={"display_name": "  Caz 🧑‍💻 ", "status_text": "On a train"}, headers=hc
    )
    assert r.status_code == 200, r.text
    me = r.json()
    assert (me["display_name"], me["status_text"], me["handle"]) == (
        "Caz 🧑‍💻",
        "On a train",
        "carol",
    )

    synced = (await client.get("/api/v1/sync", params={"since": cursor}, headers=hb)).json()
    (row,) = [u for u in synced["users"] if u["id"] == t["c"]]
    assert (row["display_name"], row["status_text"]) == ("Caz 🧑‍💻", "On a train")

    # omitted fields stay; "" clears the status line, and a status-only change syncs too
    cursor = synced["next"]
    r = await client.patch(f"{AUTH}/me", json={"status_text": ""}, headers=hc)
    assert (r.json()["display_name"], r.json()["status_text"]) == ("Caz 🧑‍💻", "")
    synced = (await client.get("/api/v1/sync", params={"since": cursor}, headers=hb)).json()
    assert [u["status_text"] for u in synced["users"] if u["id"] == t["c"]] == [""]


async def test_profile_text_is_checked(client: httpx.AsyncClient) -> None:
    h, _ = await _user(client, "alice")
    for body in (
        {"display_name": "   "},  # empty after trimming
        {"display_name": "x" * 65},
        {"status_text": "y" * 101},
        {"display_name": "evil" + chr(0x202E) + "gnp.exe"},  # right-to-left override: a spoof
        {"display_name": "two\nlines"},
        {"status_text": "tab\there"},
    ):
        r = await client.patch(f"{AUTH}/me", json=body, headers=h)
        assert r.status_code == 422, body
        assert r.json()["error"]["code"] == "profile.invalid", body
    assert (await client.get(f"{AUTH}/me", headers=h)).json()["display_name"] == "Alice"
