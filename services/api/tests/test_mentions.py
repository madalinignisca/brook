"""Mentions that survive offline (owner request, 2026-09-26).

Mentions are resolved once, at send, and stored with the message, so history and /sync
carry them: a mention received while offline still highlights. Edits don't re-resolve;
a deleted message mentions nobody. GET /channels counts unread mentions per channel.
"""

from __future__ import annotations

import httpx

from tests.test_members import _team

CH = "/api/v1/channels"


async def _send(client: httpx.AsyncClient, ch: str, body: str, h: dict[str, str]) -> dict:
    r = await client.post(f"{CH}/{ch}/messages", json={"body": body}, headers=h)
    assert r.status_code == 201, r.text
    return dict(r.json())


async def _history_row(client: httpx.AsyncClient, ch: str, mid: str, h: dict[str, str]) -> dict:
    rows = (await client.get(f"{CH}/{ch}/messages", headers=h)).json()
    (row,) = [m for m in rows if m["id"] == mid]
    return dict(row)


async def test_a_mention_received_offline_arrives_with_the_message(
    client: httpx.AsyncClient,
) -> None:
    t = await _team(client)
    hb, hc, ch = t["hb"], t["hc"], t["ch"]
    cursor = (await client.get("/api/v1/sync", params={"since": "0"}, headers=hc)).json()["next"]
    # carol is offline; bob mentions her
    sent = await _send(client, ch, "@carol can you check this?", hb)
    assert sent["mentions"] == [t["c"]]

    synced = (await client.get("/api/v1/sync", params={"since": cursor}, headers=hc)).json()
    (row,) = [m for m in synced["messages"] if m["id"] == sent["id"]]
    assert row["mentions"] == [t["c"]] and row["mention_everyone"] is False
    assert (await _history_row(client, ch, sent["id"], hc))["mentions"] == [t["c"]]


async def test_everyone_mentions_are_stored_too(client: httpx.AsyncClient) -> None:
    t = await _team(client)
    sent = await _send(client, t["ch"], "@here standup in 5", t["hb"])
    assert sent["mention_everyone"] is True
    row = await _history_row(client, t["ch"], sent["id"], t["hc"])
    assert row["mention_everyone"] is True and row["mentions"] == []


async def test_an_edit_keeps_the_mentions_it_was_sent_with(client: httpx.AsyncClient) -> None:
    # Mentions fire once, on the send: editing in "@dave" doesn't mention dave.
    t = await _team(client)
    hb, ch = t["hb"], t["ch"]
    sent = await _send(client, ch, "@carol hi", hb)
    r = await client.patch(
        f"{CH}/{ch}/messages/{sent['id']}", json={"body": "@dave hi"}, headers=hb
    )
    assert r.status_code == 200 and r.json()["mentions"] == [t["c"]]
    assert (await _history_row(client, ch, sent["id"], hb))["mentions"] == [t["c"]]


async def test_a_deleted_message_mentions_nobody(client: httpx.AsyncClient) -> None:
    t = await _team(client)
    hb, hc, ch = t["hb"], t["hc"], t["ch"]
    cursor = (await client.get("/api/v1/sync", params={"since": "0"}, headers=hc)).json()["next"]
    sent = await _send(client, ch, "@carol @channel secret", hb)
    assert (await client.delete(f"{CH}/{ch}/messages/{sent['id']}", headers=hb)).status_code == 204
    synced = (await client.get("/api/v1/sync", params={"since": cursor}, headers=hc)).json()
    (row,) = [m for m in synced["messages"] if m["id"] == sent["id"]]
    assert row["deleted_at"] is not None
    assert row["mentions"] == [] and row["mention_everyone"] is False


async def test_unread_mentions_are_counted_per_channel(client: httpx.AsyncClient) -> None:
    t = await _team(client)
    hb, hc, ch = t["hb"], t["hc"], t["ch"]

    async def carols_count() -> int:
        listed = (await client.get(CH, headers=hc)).json()
        (channel,) = [c for c in listed if c["id"] == ch]
        return int(channel["unread_mentions"])

    # carol's own message first: sending also marks the channel read up to it
    await _send(client, ch, "@carol I mention myself", hc)  # her own: never counted
    await _send(client, ch, "@carol one", hb)
    await _send(client, ch, "@channel two", hb)
    await _send(client, ch, "no mention", hb)
    await _send(client, ch, "@nobody-here", hb)  # not a member: nobody mentioned
    assert await carols_count() == 2
    newest = (await client.get(f"{CH}/{ch}/messages", headers=hc)).json()[-1]["id"]
    assert (
        await client.post(f"{CH}/{ch}/read", json={"message_id": newest}, headers=hc)
    ).status_code == 204
    assert await carols_count() == 0


async def test_mentions_list_in_the_same_order_everywhere(client: httpx.AsyncClient) -> None:
    # A cache compares rows: the same message must list its mentions identically in its
    # live answer and in later reads, or every read looks like a change.
    t = await _team(client)
    sent = await _send(client, t["ch"], "@dave @carol @bob all of you", t["hb"])
    expected = sorted([t["b"], t["c"], t["d"]])
    assert sent["mentions"] == expected
    assert (await _history_row(client, t["ch"], sent["id"], t["hc"]))["mentions"] == expected
