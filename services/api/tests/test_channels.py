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


async def test_unread_count_and_mark_read(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)
    dm = (
        await client.post(
            f"{API}/channels", json={"kind": "dm", "member": "bob"}, headers=_auth(alice)
        )
    ).json()
    cid = dm["id"]
    for text in ("one", "two"):
        await client.post(
            f"{API}/channels/{cid}/messages", json={"body": text}, headers=_auth(alice)
        )

    async def unread_for(token: str) -> int:
        chans = (await client.get(f"{API}/channels", headers=_auth(token))).json()
        return int(next(c for c in chans if c["id"] == cid)["unread_count"])

    # The author auto-reads their own messages; the recipient has 2 unread.
    assert await unread_for(alice) == 0
    assert await unread_for(bob) == 2

    # Marking read (latest) clears bob's unread.
    r = await client.post(f"{API}/channels/{cid}/read", json={}, headers=_auth(bob))
    assert r.status_code == 204
    assert await unread_for(bob) == 0


async def test_added_member_starts_caught_up(client: httpx.AsyncClient) -> None:
    """A member added to a channel with history isn't flooded with unread."""
    alice, _bob = await _two_users(client)
    await _register(client, "carol", headers=_auth(alice))
    carol = await _token(client, "carol")
    chan = (
        await client.post(
            f"{API}/channels", json={"kind": "channel", "name": "general"}, headers=_auth(alice)
        )
    ).json()
    cid = chan["id"]
    for text in ("a", "b", "c"):
        await client.post(
            f"{API}/channels/{cid}/messages", json={"body": text}, headers=_auth(alice)
        )

    await client.post(
        f"{API}/channels/{cid}/members", json={"handle": "carol"}, headers=_auth(alice)
    )
    chans = (await client.get(f"{API}/channels", headers=_auth(carol))).json()
    assert next(c for c in chans if c["id"] == cid)["unread_count"] == 0


async def test_mark_read_rejects_foreign_message(client: httpx.AsyncClient) -> None:
    import uuid

    alice, bob = await _two_users(client)
    dm = (
        await client.post(
            f"{API}/channels", json={"kind": "dm", "member": "bob"}, headers=_auth(alice)
        )
    ).json()
    resp = await client.post(
        f"{API}/channels/{dm['id']}/read",
        json={"message_id": str(uuid.uuid4())},
        headers=_auth(bob),
    )
    assert resp.status_code == 422


async def _dm_with_message(client: httpx.AsyncClient, alice: str, bob: str) -> tuple[str, str]:
    """An alice↔bob DM with one message from alice; returns (channel_id, message_id)."""
    dm = (
        await client.post(
            f"{API}/channels", json={"kind": "dm", "member": "bob"}, headers=_auth(alice)
        )
    ).json()
    msg = (
        await client.post(
            f"{API}/channels/{dm['id']}/messages", json={"body": "original"}, headers=_auth(alice)
        )
    ).json()
    return dm["id"], msg["id"]


async def test_author_edits_message(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)
    cid, mid = await _dm_with_message(client, alice, bob)

    edited = await client.patch(
        f"{API}/channels/{cid}/messages/{mid}", json={"body": "fixed"}, headers=_auth(alice)
    )
    assert edited.status_code == 200
    assert edited.json()["body"] == "fixed"
    assert edited.json()["edited_at"] is not None

    hist = await client.get(f"{API}/channels/{cid}/messages", headers=_auth(bob))
    assert [m["body"] for m in hist.json()] == ["fixed"]


async def test_non_author_cannot_edit(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)
    cid, mid = await _dm_with_message(client, alice, bob)
    resp = await client.patch(
        f"{API}/channels/{cid}/messages/{mid}", json={"body": "hijack"}, headers=_auth(bob)
    )
    assert resp.status_code == 403


async def test_author_deletes_message(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)
    cid, mid = await _dm_with_message(client, alice, bob)

    deleted = await client.delete(f"{API}/channels/{cid}/messages/{mid}", headers=_auth(alice))
    assert deleted.status_code == 204

    hist = await client.get(f"{API}/channels/{cid}/messages", headers=_auth(bob))
    assert hist.json() == []
    # editing a deleted message is a 404
    assert (
        await client.patch(
            f"{API}/channels/{cid}/messages/{mid}", json={"body": "x"}, headers=_auth(alice)
        )
    ).status_code == 404


async def test_admin_can_delete_others_message_but_member_cannot(client: httpx.AsyncClient) -> None:
    # alice is the admin (first user); bob is a member. bob posts in a shared channel.
    alice, bob = await _two_users(client)
    chan = (
        await client.post(
            f"{API}/channels", json={"kind": "channel", "name": "general"}, headers=_auth(alice)
        )
    ).json()
    cid = chan["id"]
    await client.post(f"{API}/channels/{cid}/members", json={"handle": "bob"}, headers=_auth(alice))
    mid = (
        await client.post(
            f"{API}/channels/{cid}/messages", json={"body": "bob's msg"}, headers=_auth(bob)
        )
    ).json()["id"]

    # a non-author, non-admin can't delete it
    await _register(client, "carol", headers=_auth(alice))
    carol = await _token(client, "carol")
    await client.post(
        f"{API}/channels/{cid}/members", json={"handle": "carol"}, headers=_auth(alice)
    )
    assert (
        await client.delete(f"{API}/channels/{cid}/messages/{mid}", headers=_auth(carol))
    ).status_code == 403

    # the admin (alice) can
    assert (
        await client.delete(f"{API}/channels/{cid}/messages/{mid}", headers=_auth(alice))
    ).status_code == 204


async def test_quote_reply(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)
    cid, mid = await _dm_with_message(client, alice, bob)

    replied = await client.post(
        f"{API}/channels/{cid}/messages",
        json={"body": "replying!", "reply_to_id": mid},
        headers=_auth(bob),
    )
    assert replied.status_code == 201
    data = replied.json()
    assert data["reply_to_id"] == mid
    assert data["reply_to"]["body"] == "original"
    assert data["reply_to"]["author_handle"] == "alice"

    # history carries the resolved excerpt too
    hist = await client.get(f"{API}/channels/{cid}/messages", headers=_auth(alice))
    reply_msg = next(m for m in hist.json() if m["id"] == data["id"])
    assert reply_msg["reply_to"]["body"] == "original"


async def test_reply_to_foreign_message_rejected(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)
    cid, _mid = await _dm_with_message(client, alice, bob)
    # a message id from a different channel
    other = (
        await client.post(
            f"{API}/channels", json={"kind": "channel", "name": "general"}, headers=_auth(alice)
        )
    ).json()["id"]
    foreign = (
        await client.post(
            f"{API}/channels/{other}/messages", json={"body": "elsewhere"}, headers=_auth(alice)
        )
    ).json()["id"]
    resp = await client.post(
        f"{API}/channels/{cid}/messages",
        json={"body": "bad reply", "reply_to_id": foreign},
        headers=_auth(alice),
    )
    assert resp.status_code == 404


async def test_reactions_toggle_and_aggregate(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)
    cid, mid = await _dm_with_message(client, alice, bob)
    url = f"{API}/channels/{cid}/messages/{mid}/reactions"

    # alice reacts 👍 -> count 1, me True for alice
    r = await client.post(url, json={"emoji": "👍"}, headers=_auth(alice))
    assert r.status_code == 200
    assert r.json() == [{"emoji": "👍", "count": 1, "me": True}]

    # bob reacts 👍 too -> count 2
    r = await client.post(url, json={"emoji": "👍"}, headers=_auth(bob))
    assert r.json() == [{"emoji": "👍", "count": 2, "me": True}]

    # history shows the tally; me reflects the caller
    hist = await client.get(f"{API}/channels/{cid}/messages", headers=_auth(alice))
    msg = next(m for m in hist.json() if m["id"] == mid)
    assert msg["reactions"] == [{"emoji": "👍", "count": 2, "me": True}]

    # alice toggles 👍 off -> bob's still counts; alice's me is now False
    r = await client.post(url, json={"emoji": "👍"}, headers=_auth(alice))
    assert r.json() == [{"emoji": "👍", "count": 1, "me": False}]
    hist = await client.get(f"{API}/channels/{cid}/messages", headers=_auth(bob))
    msg = next(m for m in hist.json() if m["id"] == mid)
    assert msg["reactions"] == [{"emoji": "👍", "count": 1, "me": True}]


async def test_reaction_requires_membership(client: httpx.AsyncClient) -> None:
    alice, _bob = await _two_users(client)
    cid, mid = await _dm_with_message(client, alice, _bob)
    await _register(client, "carol", headers=_auth(alice))
    carol = await _token(client, "carol")
    resp = await client.post(
        f"{API}/channels/{cid}/messages/{mid}/reactions",
        json={"emoji": "👍"},
        headers=_auth(carol),
    )
    assert resp.status_code == 404  # non-member can't see/react in the channel


async def _admin_channel(client: httpx.AsyncClient, alice: str, public: bool = False) -> str:
    chan = await client.post(
        f"{API}/channels",
        json={"kind": "channel", "name": "general", "public": public},
        headers=_auth(alice),
    )
    return chan.json()["id"]


async def test_rename_and_archive_channel(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)
    cid = await _admin_channel(client, alice)

    renamed = await client.patch(
        f"{API}/channels/{cid}", json={"name": "renamed", "topic": "hi"}, headers=_auth(alice)
    )
    assert renamed.status_code == 200
    assert renamed.json()["name"] == "renamed"
    assert renamed.json()["topic"] == "hi"

    # archive -> sending is blocked
    await client.patch(f"{API}/channels/{cid}", json={"archived": True}, headers=_auth(alice))
    blocked = await client.post(
        f"{API}/channels/{cid}/messages", json={"body": "hi"}, headers=_auth(alice)
    )
    assert blocked.status_code == 403
    # unarchive -> sending works again
    await client.patch(f"{API}/channels/{cid}", json={"archived": False}, headers=_auth(alice))
    assert (
        await client.post(
            f"{API}/channels/{cid}/messages", json={"body": "hi"}, headers=_auth(alice)
        )
    ).status_code == 201


async def test_non_owner_cannot_manage_channel(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)
    cid = await _admin_channel(client, alice)
    await client.post(f"{API}/channels/{cid}/members", json={"handle": "bob"}, headers=_auth(alice))
    # bob is a plain member, not owner/admin
    assert (
        await client.patch(f"{API}/channels/{cid}", json={"name": "x"}, headers=_auth(bob))
    ).status_code == 403
    assert (await client.delete(f"{API}/channels/{cid}", headers=_auth(bob))).status_code == 403


async def test_delete_channel(client: httpx.AsyncClient) -> None:
    alice, _bob = await _two_users(client)
    cid = await _admin_channel(client, alice)
    assert (await client.delete(f"{API}/channels/{cid}", headers=_auth(alice))).status_code == 204
    listed = await client.get(f"{API}/channels", headers=_auth(alice))
    assert all(c["id"] != cid for c in listed.json())


async def test_public_channel_browse_and_join(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)
    cid = await _admin_channel(client, alice, public=True)

    # bob (not a member) sees it in the public list
    pub = await client.get(f"{API}/channels/public", headers=_auth(bob))
    assert cid in [c["id"] for c in pub.json()]
    assert pub.json()[0]["public"] is True

    # bob joins -> now in his channel list, and no longer in the public (unjoined) list
    joined = await client.post(f"{API}/channels/{cid}/join", headers=_auth(bob))
    assert joined.status_code == 200
    assert cid in [
        c["id"] for c in (await client.get(f"{API}/channels", headers=_auth(bob))).json()
    ]
    assert cid not in [
        c["id"] for c in (await client.get(f"{API}/channels/public", headers=_auth(bob))).json()
    ]


async def test_cannot_join_private_channel(client: httpx.AsyncClient) -> None:
    alice, bob = await _two_users(client)
    cid = await _admin_channel(client, alice, public=False)
    assert (await client.post(f"{API}/channels/{cid}/join", headers=_auth(bob))).status_code == 404
