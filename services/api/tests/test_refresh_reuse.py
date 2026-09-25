"""Refresh-token reuse: families and the crash grace (routers/auth.py `_rotate`).

A rotated token presented again is either a client that crashed after our rotation
but before saving its successor, or a copy in someone else's hands. Within the grace
window, while the successor is unused, it's the crash: the device stays signed in.
Otherwise it's theft: that login's whole chain is revoked, and nothing else.
"""

from __future__ import annotations

from datetime import timedelta

import httpx
from sqlalchemy import func, select, update

from app import db
from app.models import AuthEvent, RefreshToken, utcnow
from app.security import hash_token

AUTH = "/api/v1/auth"
PW = "supersecret"


async def _login(client: httpx.AsyncClient) -> str:
    r = await client.post(f"{AUTH}/login", json={"handle": "alice", "password": PW})
    assert r.status_code == 200, r.text
    return str(r.json()["refresh_token"])


async def _alice(client: httpx.AsyncClient) -> None:
    body = {"handle": "alice", "display_name": "Alice", "password": PW}
    assert (await client.post(f"{AUTH}/register", json=body)).status_code == 201


async def _refresh(client: httpx.AsyncClient, token: str) -> httpx.Response:
    return await client.post(f"{AUTH}/refresh", json={"refresh_token": token})


async def _events(kind: str) -> int:
    async with db.get_sessionmaker()() as s:
        n = await s.scalar(
            select(func.count()).select_from(AuthEvent).where(AuthEvent.kind == kind)
        )
        return int(n or 0)


async def _reuse_events() -> int:
    return await _events("refresh_token_reuse")


async def _rotated_ago(raw: str, ago: timedelta) -> None:
    async with db.get_sessionmaker()() as session:
        await session.execute(
            update(RefreshToken)
            .where(RefreshToken.token_hash == hash_token(raw))
            .values(rotated_at=utcnow() - ago)
        )
        await session.commit()


async def test_a_lost_reply_then_a_long_offline_stretch_stays_signed_in(
    client: httpx.AsyncClient,
) -> None:
    # The reply carrying S was lost as the train entered a tunnel; the phone comes
    # back the next morning and retries with T.
    await _alice(client)
    t = await _login(client)
    assert (await _refresh(client, t)).status_code == 200  # S, never received
    await _rotated_ago(t, timedelta(hours=23))
    again = await _refresh(client, t)
    assert again.status_code == 200
    assert (await _refresh(client, again.json()["refresh_token"])).status_code == 200
    assert await _reuse_events() == 0


async def test_a_successor_retired_by_the_grace_is_reuse(client: httpx.AsyncClient) -> None:
    # A thief replays T and takes the grace; the device then presents S, which the
    # grace retired. That must end the family for both, not start a second grace
    # (which would let the two retire each other's token for a whole day, uncaught).
    await _alice(client)
    t = await _login(client)
    s = (await _refresh(client, t)).json()["refresh_token"]
    u = (await _refresh(client, t)).json()["refresh_token"]  # the thief's grace
    assert (await _refresh(client, s)).status_code == 401  # the device: reuse
    assert (await _refresh(client, u)).status_code == 401  # the thief's chain is dead
    assert await _reuse_events() == 1


async def test_a_crash_replay_within_grace_keeps_the_device_signed_in(
    client: httpx.AsyncClient,
) -> None:
    await _alice(client)
    t = await _login(client)
    assert (await _refresh(client, t)).status_code == 200  # S minted, never saved
    again = await _refresh(client, t)  # relaunch replays T
    assert again.status_code == 200
    u = again.json()["refresh_token"]
    assert (await _refresh(client, u)).status_code == 200  # and the chain goes on
    assert await _reuse_events() == 0
    assert await _events("refresh_token_grace") == 1  # every grace use leaves a trace


async def test_reuse_after_the_successor_was_used_revokes_that_family_only(
    client: httpx.AsyncClient,
) -> None:
    await _alice(client)
    other_device = await _login(client)
    t = await _login(client)
    s = (await _refresh(client, t)).json()["refresh_token"]
    v = (await _refresh(client, s)).json()["refresh_token"]  # the chain moved on

    assert (await _refresh(client, t)).status_code == 401  # a copy of T: theft
    assert (await _refresh(client, v)).status_code == 401  # the whole chain is dead
    assert (await _refresh(client, other_device)).status_code == 200  # not implicated
    assert await _reuse_events() == 1


async def test_reuse_after_the_grace_window_revokes_the_family(client: httpx.AsyncClient) -> None:
    await _alice(client)
    t = await _login(client)
    s = (await _refresh(client, t)).json()["refresh_token"]
    await _rotated_ago(t, timedelta(hours=25))  # past the 24 h window

    assert (await _refresh(client, t)).status_code == 401
    assert (await _refresh(client, s)).status_code == 401  # the unused successor too
    assert await _reuse_events() == 1


async def test_a_second_replay_is_theft(client: httpx.AsyncClient) -> None:
    # The grace hands out one replacement: the successor it retires can't be retired
    # twice, so a second copy of T lands on the theft path and ends the chain.
    await _alice(client)
    t = await _login(client)
    await _refresh(client, t)
    u = (await _refresh(client, t)).json()["refresh_token"]
    assert (await _refresh(client, t)).status_code == 401
    assert (await _refresh(client, u)).status_code == 401
    assert await _reuse_events() == 1


async def test_a_logged_out_token_is_refused_not_theft(client: httpx.AsyncClient) -> None:
    await _alice(client)
    other_device = await _login(client)
    t = await _login(client)
    assert (await client.post(f"{AUTH}/logout", json={"refresh_token": t})).status_code == 204
    assert (await _refresh(client, t)).status_code == 401
    assert (await _refresh(client, other_device)).status_code == 200
    assert await _reuse_events() == 0


async def test_a_login_starts_a_new_family_and_rotation_keeps_it(
    client: httpx.AsyncClient,
) -> None:
    await _alice(client)
    a, b = await _login(client), await _login(client)
    a2 = (await _refresh(client, a)).json()["refresh_token"]
    async with db.get_sessionmaker()() as s:
        fam = {
            raw: await s.scalar(
                select(RefreshToken.family_id).where(RefreshToken.token_hash == hash_token(raw))
            )
            for raw in (a, b, a2)
        }
    assert fam[a] == fam[a2] != fam[b]


async def test_an_expired_rotated_token_still_ends_the_family(client: httpx.AsyncClient) -> None:
    # Expiry doesn't hide a reuse: the chain it started may still be live.
    await _alice(client)
    t = await _login(client)
    s = (await _refresh(client, t)).json()["refresh_token"]
    v = (await _refresh(client, s)).json()["refresh_token"]
    async with db.get_sessionmaker()() as session:
        await session.execute(
            update(RefreshToken)
            .where(RefreshToken.token_hash == hash_token(t))
            .values(expires_at=utcnow() - timedelta(seconds=1))
        )
        await session.commit()
    assert (await _refresh(client, t)).status_code == 401
    assert (await _refresh(client, v)).status_code == 401
    assert await _reuse_events() == 1


async def test_an_expired_live_token_is_just_refused(client: httpx.AsyncClient) -> None:
    await _alice(client)
    t = await _login(client)
    async with db.get_sessionmaker()() as session:
        await session.execute(
            update(RefreshToken)
            .where(RefreshToken.token_hash == hash_token(t))
            .values(expires_at=utcnow() - timedelta(seconds=1))
        )
        await session.commit()
    assert (await _refresh(client, t)).status_code == 401
    assert await _reuse_events() == 0


async def test_logout_ends_the_successor_of_a_refresh_in_flight(
    client: httpx.AsyncClient,
) -> None:
    # Sign-out raced a refresh: the server rotated T into S, the client (signing out)
    # never kept S and logs out with T. S must not stay live for its whole TTL.
    await _alice(client)
    other_device = await _login(client)
    t = await _login(client)
    s = (await _refresh(client, t)).json()["refresh_token"]
    assert (await client.post(f"{AUTH}/logout", json={"refresh_token": t})).status_code == 204
    assert (await _refresh(client, s)).status_code == 401
    assert (await _refresh(client, other_device)).status_code == 200  # another family
    assert await _reuse_events() == 0  # a logged-out token is refused, not theft
    assert await _events("logout") == 1  # on record, whoever held the token


async def test_logout_with_an_unknown_token_is_a_quiet_204(client: httpx.AsyncClient) -> None:
    await _alice(client)
    live = await _login(client)
    r = await client.post(f"{AUTH}/logout", json={"refresh_token": "not-a-token"})
    assert r.status_code == 204
    assert (await _refresh(client, live)).status_code == 200
