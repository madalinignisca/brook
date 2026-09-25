"""/sync (sync spec 2026-09-25 §6): stamping, scope, state-only first sync, new
channels arriving complete, reset, paging, and seq on live events."""

from __future__ import annotations

import uuid

import httpx
from fastapi.testclient import TestClient
from sqlalchemy import select, update

from app import db
from app.models import Channel, Membership

AUTH = "/api/v1/auth"
PW = "supersecret"


def _h(token: str) -> dict[str, str]:
    return {"Authorization": f"Bearer {token}"}


async def _user(
    client: httpx.AsyncClient, handle: str, admin: dict[str, str] | None = None
) -> dict[str, str]:
    body = {"handle": handle, "display_name": handle.title(), "password": PW}
    await client.post(f"{AUTH}/register", json=body, headers=admin or {})
    r = await client.post(f"{AUTH}/login", json={"handle": handle, "password": PW})
    return _h(r.json()["access_token"])


async def _sync(
    client: httpx.AsyncClient, h: dict[str, str], since: str = "0", **kw: object
) -> dict:
    r = await client.get("/api/v1/sync", params={"since": since, **kw}, headers=h)
    assert r.status_code == 200, r.text
    return dict(r.json())


async def _setup(client: httpx.AsyncClient) -> tuple[dict[str, str], dict[str, str], str]:
    ha = await _user(client, "alice")
    hb = await _user(client, "bob", ha)
    ch = (
        await client.post("/api/v1/channels", json={"kind": "channel", "name": "g"}, headers=ha)
    ).json()
    await client.post(f"/api/v1/channels/{ch['id']}/members", json={"handle": "bob"}, headers=ha)
    return ha, hb, ch["id"]


async def test_first_sync_is_state_only(client: httpx.AsyncClient) -> None:
    ha, hb, ch = await _setup(client)
    await client.post(f"/api/v1/channels/{ch}/messages", json={"body": "old"}, headers=ha)
    first = await _sync(client, hb)
    assert first["messages"] == []  # history is paged with before=, not synced
    assert [c["id"] for c in first["channels"]] == [ch]
    assert {u["handle"] for u in first["users"]} == {"alice", "bob"}
    assert int(first["next"]) > 0 and first["more"] is False
    await client.post(f"/api/v1/channels/{ch}/messages", json={"body": "new"}, headers=ha)
    later = await _sync(client, hb, first["next"])
    assert [m["body"] for m in later["messages"]] == ["new"]  # only after the cursor


async def test_every_change_bumps_seq_and_arrives(client: httpx.AsyncClient) -> None:
    ha, hb, ch = await _setup(client)
    cursor = (await _sync(client, hb))["next"]
    sent = (
        await client.post(f"/api/v1/channels/{ch}/messages", json={"body": "hi"}, headers=ha)
    ).json()
    s1 = await _sync(client, hb, cursor)
    assert [m["id"] for m in s1["messages"]] == [sent["id"]] and s1["messages"][0]["seq"] > int(
        cursor
    )

    await client.patch(
        f"/api/v1/channels/{ch}/messages/{sent['id']}", json={"body": "edited"}, headers=ha
    )
    s2 = await _sync(client, hb, s1["next"])
    assert s2["messages"][0]["body"] == "edited"

    await client.post(
        f"/api/v1/channels/{ch}/messages/{sent['id']}/reactions", json={"emoji": "👍"}, headers=hb
    )
    s3 = await _sync(client, hb, s2["next"])
    assert [m["id"] for m in s3["messages"]] == [sent["id"]]  # a reaction re-stamps the message

    await client.patch(f"/api/v1/channels/{ch}", json={"name": "renamed"}, headers=ha)
    s4 = await _sync(client, hb, s3["next"])
    assert [c["name"] for c in s4["channels"]] == ["renamed"]

    await client.delete(f"/api/v1/channels/{ch}/messages/{sent['id']}", headers=ha)
    s5 = await _sync(client, hb, s4["next"])
    tomb = s5["messages"][0]
    assert tomb["deleted_at"] is not None and tomb["body"] == "" and tomb["reactions"] == []


async def test_read_state_is_private(client: httpx.AsyncClient) -> None:
    ha, hb, ch = await _setup(client)
    sent = (
        await client.post(f"/api/v1/channels/{ch}/messages", json={"body": "hi"}, headers=ha)
    ).json()
    cursor = (await _sync(client, hb))["next"]
    await client.post(f"/api/v1/channels/{ch}/read", json={"message_id": sent["id"]}, headers=hb)
    mine = await _sync(client, hb, cursor)
    bob_rows = [m for m in mine["memberships"] if m["last_read_message_id"]]
    assert [m["last_read_message_id"] for m in bob_rows] == [sent["id"]]
    theirs = await _sync(client, ha, cursor)
    assert all(m["last_read_message_id"] is None for m in theirs["memberships"])


async def test_nothing_from_channels_im_not_in(client: httpx.AsyncClient) -> None:
    ha, hb, _ch = await _setup(client)
    cursor = (await _sync(client, hb))["next"]
    secret = (
        await client.post(
            "/api/v1/channels", json={"kind": "channel", "name": "admins"}, headers=ha
        )
    ).json()
    await client.post(f"/api/v1/channels/{secret['id']}/messages", json={"body": "x"}, headers=ha)
    page = await _sync(client, hb, cursor)
    assert page["channels"] == [] and page["messages"] == []


async def test_a_new_channel_arrives_with_all_its_members(client: httpx.AsyncClient) -> None:
    ha = await _user(client, "alice")
    hb = await _user(client, "bob", ha)
    await _user(client, "carol", ha)
    ch = (
        await client.post("/api/v1/channels", json={"kind": "channel", "name": "old"}, headers=ha)
    ).json()
    await client.post(f"/api/v1/channels/{ch['id']}/members", json={"handle": "carol"}, headers=ha)
    cursor = (await _sync(client, hb))["next"]  # bob isn't in it yet
    await client.post(f"/api/v1/channels/{ch['id']}/members", json={"handle": "bob"}, headers=ha)
    page = await _sync(client, hb, cursor)
    assert [c["id"] for c in page["channels"]] == [ch["id"]]
    members = {m["user_id"] for m in page["memberships"] if m["channel_id"] == ch["id"]}
    assert len(members) == 3  # alice and carol too, though their rows predate the cursor
    assert {u["handle"] for u in page["users"]} >= {"alice", "bob", "carol"}


async def test_removal_and_leaving_are_tombstoned(client: httpx.AsyncClient) -> None:
    ha, hb, ch = await _setup(client)
    cursor_b = (await _sync(client, hb))["next"]
    cursor_a = (await _sync(client, ha))["next"]
    async with db.get_sessionmaker()() as s:  # no member-removal route yet: ORM delete
        member = await s.get(
            Membership,
            (uuid.UUID(ch), uuid.UUID((await client.get(f"{AUTH}/me", headers=hb)).json()["id"])),
        )
        assert member is not None
        await s.delete(member)
        await s.commit()
    bob = await _sync(client, hb, cursor_b)
    assert [r["channel_id"] for r in bob["removed_channels"]] == [ch]
    alice = await _sync(client, ha, cursor_a)
    assert [(x["channel_id"]) for x in alice["left_members"]] == [ch]


async def test_channel_delete_tells_every_member(client: httpx.AsyncClient) -> None:
    ha, hb, ch = await _setup(client)
    cursor = (await _sync(client, hb))["next"]
    assert (await client.delete(f"/api/v1/channels/{ch}", headers=ha)).status_code == 204
    page = await _sync(client, hb, cursor)
    assert [r["channel_id"] for r in page["removed_channels"]] == [ch]


async def test_unknown_cursor_is_410_reset(client: httpx.AsyncClient) -> None:
    ha, _hb, _ch = await _setup(client)
    for bad in ("999999999", "abc", "-1"):
        r = await client.get("/api/v1/sync", params={"since": bad}, headers=ha)
        assert r.status_code == 410 and r.json()["error"]["code"] == "sync.reset", bad


async def test_paging_walks_to_the_end_without_splitting_a_seq(client: httpx.AsyncClient) -> None:
    ha, hb, ch = await _setup(client)
    cursor = (await _sync(client, hb))["next"]
    for i in range(7):
        await client.post(f"/api/v1/channels/{ch}/messages", json={"body": str(i)}, headers=ha)
    got: list[str] = []
    pages = 0
    while True:
        page = await _sync(client, hb, cursor, limit=3)
        pages += 1
        got += [m["body"] for m in page["messages"]]
        seqs = [m["seq"] for m in page["messages"]]
        assert all(int(cursor) < q <= int(page["next"]) for q in seqs)
        cursor = page["next"]
        if not page["more"]:
            break
    assert got == [str(i) for i in range(7)] and pages == 3


async def test_archive_is_a_channel_change(client: httpx.AsyncClient) -> None:
    ha, hb, ch = await _setup(client)
    cursor = (await _sync(client, hb))["next"]
    async with db.get_sessionmaker()() as s:
        await s.execute(update(Channel).where(Channel.id == uuid.UUID(ch)).values(public=True))
        await s.commit()  # a bulk UPDATE bypasses the ORM hook: not how routes write
    page = await _sync(client, hb, cursor)
    assert page["channels"] == []  # documents the limit: stamping is ORM-level
    await client.patch(f"/api/v1/channels/{ch}", json={"archived": True}, headers=ha)
    page = await _sync(client, hb, cursor)
    assert [c["archived"] for c in page["channels"]] == [True]


def test_live_events_carry_seq(sync_client: TestClient) -> None:
    http = sync_client
    http.post(f"{AUTH}/register", json={"handle": "alice", "display_name": "A", "password": PW})
    a = http.post(f"{AUTH}/login", json={"handle": "alice", "password": PW}).json()
    ha = _h(a["access_token"])
    ch = http.post("/api/v1/channels", json={"kind": "channel", "name": "g"}, headers=ha).json()
    with http.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": a["access_token"]}})
        assert ws.receive_json()["type"] == "ready"
        sent = http.post(
            f"/api/v1/channels/{ch['id']}/messages", json={"body": "hi"}, headers=ha
        ).json()
        seen: dict[str, dict] = {}
        while "message.new" not in seen:
            ev = ws.receive_json()
            seen[ev["type"]] = ev["data"]
        assert seen["message.new"]["seq"] == sent["seq"] > 0
        http.delete(f"/api/v1/channels/{ch['id']}/messages/{sent['id']}", headers=ha)
        while "message.delete" not in seen:
            ev = ws.receive_json()
            seen[ev["type"]] = ev["data"]
        assert seen["message.delete"]["seq"] > sent["seq"]


async def test_a_send_does_not_resend_the_member_list(client: httpx.AsyncClient) -> None:
    """My membership row moves with every read-marker update (a send is one); that
    must not look like joining, which pulls in the whole member list and profiles."""
    ha, _hb, ch = await _setup(client)
    cursor = (await _sync(client, ha))["next"]
    await client.post(f"/api/v1/channels/{ch}/messages", json={"body": "hi"}, headers=ha)
    page = await _sync(client, ha, cursor)
    me = (await client.get(f"{AUTH}/me", headers=ha)).json()["id"]
    assert [m["user_id"] for m in page["memberships"]] == [me]  # only my read marker
    assert page["users"] == [] and page["channels"] == []


async def test_a_rolled_back_savepoint_forgets_its_seq(client: httpx.AsyncClient) -> None:
    """The savepoint that took the seq rolled back (e.g. a duplicate client_id): the
    counter bump and its lock are gone, so the cached number must not be reused."""
    from app.models import Message
    from app.sync import transaction_seq

    ha, _hb, ch = await _setup(client)
    me = uuid.UUID((await client.get(f"{AUTH}/me", headers=ha)).json()["id"])
    async with db.get_sessionmaker()() as s:
        nested = await s.begin_nested()
        s.add(Message(channel_id=uuid.UUID(ch), author_id=me, body="x"))
        await s.flush()
        assert transaction_seq(s.sync_session) > 0  # taken inside the savepoint
        await nested.rollback()
        assert transaction_seq(s.sync_session) == 0  # forgotten with the savepoint
        s.add(Message(channel_id=uuid.UUID(ch), author_id=me, body="y"))
        await s.flush()  # takes (and locks) a fresh one
        assert transaction_seq(s.sync_session) > 0
        await s.rollback()


# ---------------------------------------------------------------- auth review of #91


async def test_strangers_are_never_in_users(client: httpx.AsyncClient) -> None:
    from app.models import User

    ha, hb, _ch = await _setup(client)
    await _user(client, "carol", ha)  # shares no channel with bob
    first = await _sync(client, hb)
    assert "carol" not in {u["handle"] for u in first["users"]}
    async with db.get_sessionmaker()() as s:  # carol's profile changes (stamped)
        carol = (await s.scalars(select(User).where(User.handle == "carol"))).one()
        carol.display_name = "Carol Renamed"
        await s.commit()
    later = await _sync(client, hb, first["next"])
    assert later["users"] == []


async def test_other_peoples_removals_elsewhere_never_leak(client: httpx.AsyncClient) -> None:
    ha, hb, _ch = await _setup(client)
    await _user(client, "carol", ha)
    other = (
        await client.post("/api/v1/channels", json={"kind": "channel", "name": "x"}, headers=ha)
    ).json()
    await client.post(
        f"/api/v1/channels/{other['id']}/members", json={"handle": "carol"}, headers=ha
    )
    cursor = (await _sync(client, hb))["next"]
    carol_id = (await client.get(f"{AUTH}/me", headers=await _user(client, "carol"))).json()["id"]
    async with db.get_sessionmaker()() as s:
        m = await s.get(Membership, (uuid.UUID(other["id"]), uuid.UUID(carol_id)))
        assert m is not None
        await s.delete(m)
        await s.commit()
    page = await _sync(client, hb, cursor)
    assert page["left_members"] == [] and page["removed_channels"] == []


async def test_removed_then_readded_is_not_reported_removed(client: httpx.AsyncClient) -> None:
    ha, hb, ch = await _setup(client)
    cursor = (await _sync(client, hb))["next"]
    bob_id = (await client.get(f"{AUTH}/me", headers=hb)).json()["id"]
    async with db.get_sessionmaker()() as s:
        m = await s.get(Membership, (uuid.UUID(ch), uuid.UUID(bob_id)))
        assert m is not None
        await s.delete(m)
        await s.commit()
    await client.post(f"/api/v1/channels/{ch}/members", json={"handle": "bob"}, headers=ha)
    page = await _sync(client, hb, cursor)
    assert page["removed_channels"] == []
    assert [c["id"] for c in page["channels"]] == [ch]


async def test_a_password_change_is_invisible_to_co_members(client: httpx.AsyncClient) -> None:
    ha, hb, _ch = await _setup(client)
    cursor = (await _sync(client, hb))["next"]
    r = await client.post(
        f"{AUTH}/password",
        json={
            "current_password": PW,
            "new_password": "brand-new-pass",
            "sign_out_other_devices": False,
        },
        headers=ha,
    )
    assert r.status_code == 200
    page = await _sync(client, hb, cursor)
    assert page["users"] == []  # nothing visible changed, so nothing to infer


async def test_non_ascii_digit_cursor_is_a_reset(client: httpx.AsyncClient) -> None:
    ha, _hb, _ch = await _setup(client)
    r = await client.get("/api/v1/sync", params={"since": "²"}, headers=ha)
    assert r.status_code == 410


def test_reaction_and_channel_events_carry_a_fresh_seq(sync_client: TestClient) -> None:
    http = sync_client
    http.post(f"{AUTH}/register", json={"handle": "alice", "display_name": "A", "password": PW})
    a = http.post(f"{AUTH}/login", json={"handle": "alice", "password": PW}).json()
    ha = _h(a["access_token"])
    http.post(
        f"{AUTH}/register", json={"handle": "bob", "display_name": "B", "password": PW}, headers=ha
    )
    ch = http.post("/api/v1/channels", json={"kind": "channel", "name": "g"}, headers=ha).json()
    with http.websocket_connect("/ws") as ws:
        ws.send_json({"type": "auth", "data": {"access_token": a["access_token"]}})
        assert ws.receive_json()["type"] == "ready"
        sent = http.post(
            f"/api/v1/channels/{ch['id']}/messages", json={"body": "hi"}, headers=ha
        ).json()
        http.post(
            f"/api/v1/channels/{ch['id']}/messages/{sent['id']}/reactions",
            json={"emoji": "👍"},
            headers=ha,
        )
        http.post(f"/api/v1/channels/{ch['id']}/members", json={"handle": "bob"}, headers=ha)
        seen: dict[str, dict] = {}
        while "channel.update" not in seen:
            ev = ws.receive_json()
            seen[ev["type"]] = ev["data"]
        assert seen["reaction.update"]["seq"] > sent["seq"]
        assert seen["channel.update"]["seq"] > seen["reaction.update"]["seq"]
