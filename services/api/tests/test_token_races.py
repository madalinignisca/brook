"""Token issue/revoke ordering under concurrency: Postgres only.

The race these guard (see routers/auth.lock_user): under READ COMMITTED a
"revoke all" UPDATE does not see a refresh token committed after the statement
started, so a refresh rotating in parallel with a password change would leave
the attacker a live token. SQLite serialises writers and cannot show it, so
these run only against the Postgres CI service (BROOK_TEST_DATABASE_URL), an
explicitly declared precondition, not a skip on something that looks missing.
"""

from __future__ import annotations

import asyncio
import os
import uuid
from datetime import timedelta

import httpx
import pytest
from sqlalchemy import select, update

from app import db
from app.models import RefreshToken, User, utcnow
from app.routers.auth import lock_user
from app.security import hash_token, new_refresh_token

pytestmark = pytest.mark.skipif(
    not os.environ.get("BROOK_TEST_DATABASE_URL", "").startswith("postgresql"),
    reason="needs Postgres (BROOK_TEST_DATABASE_URL); SQLite serialises writers",
)

AUTH = "/api/v1/auth"
PW = "supersecret"
# Long enough for a request that is NOT blocked to finish; short enough to keep
# the suite fast. A blocked request stays pending for as long as we hold the lock.
SETTLE = 0.5


async def _alice(client: httpx.AsyncClient) -> tuple[uuid.UUID, dict[str, str]]:
    body = {"handle": "alice", "display_name": "Alice", "password": PW}
    await client.post(f"{AUTH}/register", json=body)
    pair = (await client.post(f"{AUTH}/login", json={"handle": "alice", "password": PW})).json()
    me = await client.get(f"{AUTH}/me", headers={"Authorization": f"Bearer {pair['access_token']}"})
    return uuid.UUID(me.json()["id"]), pair


async def test_revoke_all_catches_token_from_inflight_refresh(client: httpx.AsyncClient) -> None:
    user_id, pair = await _alice(client)

    # Session A is a refresh caught mid-flight, doing exactly what /auth/refresh
    # does: lock the user, revoke the old token, insert the new one, not committed.
    async with db.get_sessionmaker()() as a:
        await lock_user(a, user_id)
        old = await a.scalar(
            select(RefreshToken).where(RefreshToken.token_hash == hash_token(pair["refresh_token"]))
        )
        assert old is not None
        await a.execute(update(RefreshToken).where(RefreshToken.id == old.id).values(revoked=True))
        raw_new, new_hash = new_refresh_token()
        a.add(
            RefreshToken(
                user_id=user_id, token_hash=new_hash, expires_at=utcnow() + timedelta(days=1)
            )
        )
        await a.flush()

        change = asyncio.create_task(
            client.post(
                f"{AUTH}/password",
                json={"current_password": PW, "new_password": "brand-new-pass"},
                headers={"Authorization": f"Bearer {pair['access_token']}"},
            )
        )
        await asyncio.sleep(SETTLE)
        assert not change.done()  # waits for the in-flight refresh
        await a.commit()

    assert (await change).status_code == 200
    async with db.get_sessionmaker()() as check:
        new = await check.scalar(select(RefreshToken).where(RefreshToken.token_hash == new_hash))
        assert new is not None
        # The whole point: the token the racing refresh minted is revoked too.
        assert new.revoked is True
    stale = await client.post(f"{AUTH}/refresh", json={"refresh_token": raw_new})
    assert stale.status_code == 401


@pytest.mark.parametrize("route", ["refresh", "login"])
async def test_token_issuing_routes_take_the_user_lock(
    client: httpx.AsyncClient, route: str
) -> None:
    user_id, pair = await _alice(client)
    if route == "refresh":
        call = client.post(f"{AUTH}/refresh", json={"refresh_token": pair["refresh_token"]})
    else:
        call = client.post(f"{AUTH}/login", json={"handle": "alice", "password": PW})

    async with db.get_sessionmaker()() as holder:
        # Hold exactly the lock a password change holds: an UPDATE of a non-key
        # column (FOR NO KEY UPDATE). The token INSERT's foreign-key check only
        # takes FOR KEY SHARE, which does NOT conflict with it, so a route that
        # skips lock_user sails through here and mints a token the change's
        # revoke-all cannot see. That is the bug; FOR UPDATE is what conflicts.
        await holder.execute(
            update(User).where(User.id == user_id).values(display_name="Alice (changing)")
        )
        task = asyncio.create_task(call)
        await asyncio.sleep(SETTLE)
        assert not task.done()
        await holder.rollback()
    assert (await task).status_code == 200


async def test_admin_reset_takes_the_target_lock(client: httpx.AsyncClient) -> None:
    _alice_id, alice = await _alice(client)
    headers = {"Authorization": f"Bearer {alice['access_token']}"}
    bob = {"handle": "bob", "display_name": "Bob", "password": PW}
    await client.post(f"{AUTH}/register", json=bob, headers=headers)
    async with db.get_sessionmaker()() as s:
        bob_id = await s.scalar(select(User.id).where(User.handle == "bob"))
    assert bob_id is not None

    async with db.get_sessionmaker()() as holder:
        await lock_user(holder, bob_id)  # e.g. bob's device refreshing
        task = asyncio.create_task(
            client.post(
                f"/api/v1/users/{bob_id}/password",
                json={"admin_password": PW, "new_password": "temporary-pass"},
                headers=headers,
            )
        )
        await asyncio.sleep(SETTLE)
        assert not task.done()
        await holder.rollback()
    assert (await task).status_code == 204


async def test_lost_refresh_race_is_not_a_failure(client: httpx.AsyncClient) -> None:
    """Two tabs refresh the same token at once: one wins, the other gets 401, and
    the loser must not count against the IP (it presented a valid token)."""
    from app import ratelimit

    user_id, pair = await _alice(client)
    lim = ratelimit.get_limiter()
    body = {"refresh_token": pair["refresh_token"]}

    async with db.get_sessionmaker()() as holder:
        # Park both refreshes on the user lock, after each has read the token as
        # valid: that is the interleaving where the loser's CAS matches no row.
        await lock_user(holder, user_id)
        tab1 = asyncio.create_task(client.post(f"{AUTH}/refresh", json=body))
        tab2 = asyncio.create_task(client.post(f"{AUTH}/refresh", json=body))
        await asyncio.sleep(SETTLE)
        assert not tab1.done() and not tab2.done()
        await holder.rollback()
    r1, r2 = await tab1, await tab2
    assert sorted([r1.status_code, r2.status_code]) == [200, 401]
    winner = (r1 if r1.status_code == 200 else r2).json()["refresh_token"]

    # Not counted: the IP can still fail backoff_after - 1 times without a 429.
    # Had the lost race counted, the last of these would already be throttled.
    for _ in range(lim.config.backoff_after - 1):
        r = await client.post(f"{AUTH}/login", json={"handle": "alice", "password": "nope-x"})
        assert r.status_code == 401
    # And a genuinely reused token still is a failure. Its successor is used first:
    # an unused one within the grace window is a crash replay (test_refresh_reuse.py).
    assert (await client.post(f"{AUTH}/refresh", json={"refresh_token": winner})).status_code == 200
    reuse = await client.post(f"{AUTH}/refresh", json=body)
    assert reuse.status_code == 401
    throttled = await client.post(f"{AUTH}/login", json={"handle": "alice", "password": "nope-x"})
    assert throttled.status_code == 429


async def test_one_pending_token_completes_one_login(
    client: httpx.AsyncClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Two concurrent /auth/totp calls with one pending token and two different valid
    inputs (a code and a recovery code): exactly one TokenPair (TOTP spec §2.2)."""
    from urllib.parse import parse_qs, urlparse

    from app import totp as totp_core
    from app.routers import totp as totp_router

    monkeypatch.setattr(totp_router, "_now", lambda: 1_800_000_000.0)
    user_id, pair = await _alice(client)
    h = {"Authorization": f"Bearer {pair['access_token']}"}
    r = await client.post(f"{AUTH}/totp/enroll", json={"password": PW}, headers=h)
    secret = parse_qs(urlparse(r.json()["otpauth_uri"]).query)["secret"][0]
    step = totp_core.step_for(1_800_000_000.0)
    act = await client.post(
        f"{AUTH}/totp/activate", json={"code": totp_core.code_at(secret, step)}, headers=h
    )
    recovery = act.json()["recovery_codes"][0]
    login = await client.post(
        f"{AUTH}/login", json={"handle": "alice", "password": PW, "supports_totp": True}
    )
    token = login.json()["totp_token"]

    async with db.get_sessionmaker()() as holder:
        await lock_user(holder, user_id)  # park both behind the user lock
        with_code = asyncio.create_task(
            client.post(
                f"{AUTH}/totp",
                json={"totp_token": token, "code": totp_core.code_at(secret, step + 1)},
            )
        )
        with_recovery = asyncio.create_task(
            client.post(f"{AUTH}/totp", json={"totp_token": token, "recovery_code": recovery})
        )
        await asyncio.sleep(SETTLE)
        assert not with_code.done() and not with_recovery.done()
        await holder.rollback()
    codes = sorted([(await with_code).status_code, (await with_recovery).status_code])
    assert codes == [200, 403]


async def test_concurrent_resends_store_one_message(client: httpx.AsyncClient) -> None:
    """Two concurrent sends with one client_id (an outbox retry racing the original):
    one row; both answers carry its id (sync spec §4)."""
    user_id, pair = await _alice(client)
    h = {"Authorization": f"Bearer {pair['access_token']}"}
    ch = (
        await client.post("/api/v1/channels", json={"kind": "channel", "name": "g"}, headers=h)
    ).json()
    body = {"body": "once", "client_id": str(uuid.uuid4())}
    url = f"/api/v1/channels/{ch['id']}/messages"
    first, second = await asyncio.gather(
        client.post(url, json=body, headers=h), client.post(url, json=body, headers=h)
    )
    assert sorted([first.status_code, second.status_code]) == [200, 201]
    assert first.json()["id"] == second.json()["id"]
    history = (await client.get(url, headers=h)).json()
    assert [m["body"] for m in history] == ["once"]


async def test_sync_cannot_lose_a_change_committed_late(client: httpx.AsyncClient) -> None:
    """Sync spec §2, the lost-change interleaving: T1 takes a seq and stays open; T2
    must wait (the counter row is locked until T1 commits), so a /sync in between
    can't hand out a cursor past T1's change, and nothing is lost after both commit."""
    from app.models import Message

    user_id, pair = await _alice(client)
    h = {"Authorization": f"Bearer {pair['access_token']}"}
    ch = (
        await client.post("/api/v1/channels", json={"kind": "channel", "name": "g"}, headers=h)
    ).json()
    cursor = (await client.get("/api/v1/sync", params={"since": "0"}, headers=h)).json()["next"]

    async with db.get_sessionmaker()() as t1:
        t1.add(Message(channel_id=uuid.UUID(ch["id"]), author_id=user_id, body="T1"))
        await t1.flush()  # stamps: takes the counter's row lock, not committed
        t2 = asyncio.create_task(
            client.post(f"/api/v1/channels/{ch['id']}/messages", json={"body": "T2"}, headers=h)
        )
        await asyncio.sleep(SETTLE)
        assert not t2.done()  # T2 waits for the counter
        between = (await client.get("/api/v1/sync", params={"since": cursor}, headers=h)).json()
        assert between["messages"] == [] and between["next"] == cursor
        await t1.commit()
    assert (await t2).status_code == 201
    after = (await client.get("/api/v1/sync", params={"since": cursor}, headers=h)).json()
    bodies = [m["body"] for m in after["messages"]]
    assert bodies == ["T1", "T2"]  # commit order is seq order; T1 not lost
