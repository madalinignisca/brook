"""Password change (self) and admin user management (list, reset)."""

from __future__ import annotations

import httpx

AUTH = "/api/v1/auth"
USERS = "/api/v1/users"
PW = "supersecret"


async def _register(client: httpx.AsyncClient, handle: str, **kw: object) -> httpx.Response:
    body = {"handle": handle, "display_name": handle.title(), "password": PW}
    return await client.post(f"{AUTH}/register", json=body, **kw)  # type: ignore[arg-type]


async def _login(client: httpx.AsyncClient, handle: str, password: str = PW) -> httpx.Response:
    return await client.post(f"{AUTH}/login", json={"handle": handle, "password": password})


def _bearer(pair: httpx.Response) -> dict[str, str]:
    return {"Authorization": f"Bearer {pair.json()['access_token']}"}


async def _admin_and_member(client: httpx.AsyncClient) -> tuple[httpx.Response, httpx.Response]:
    """alice (admin, bootstrapped) and bob (member); returns both login responses."""
    await _register(client, "alice")
    alice = await _login(client, "alice")
    await _register(client, "bob", headers=_bearer(alice))
    return alice, await _login(client, "bob")


# ---------------------------------------------------------------- self change


async def test_change_password_signs_out_everywhere_and_keeps_caller(
    client: httpx.AsyncClient,
) -> None:
    await _register(client, "alice")
    phone = await _login(client, "alice")
    laptop = await _login(client, "alice")

    resp = await client.post(
        f"{AUTH}/password",
        json={"current_password": PW, "new_password": "brand-new-pass"},
        headers=_bearer(laptop),
    )
    assert resp.status_code == 200
    fresh = resp.json()

    # Every earlier refresh token is dead, including the caller's own old one.
    for old in (phone, laptop):
        again = await client.post(
            f"{AUTH}/refresh", json={"refresh_token": old.json()["refresh_token"]}
        )
        assert again.status_code == 401
    # The pair returned by the change works.
    assert (
        await client.post(f"{AUTH}/refresh", json={"refresh_token": fresh["refresh_token"]})
    ).status_code == 200
    # Old password no longer logs in; the new one does.
    assert (await _login(client, "alice")).status_code == 401
    assert (await _login(client, "alice", "brand-new-pass")).status_code == 200


async def test_change_password_wrong_current_is_403_not_401(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")
    pair = await _login(client, "alice")
    resp = await client.post(
        f"{AUTH}/password",
        json={"current_password": "not-it", "new_password": "brand-new-pass"},
        headers=_bearer(pair),
    )
    # 401 would make clients refresh-and-retry as if the access token expired.
    assert resp.status_code == 403
    assert resp.json()["error"]["code"] == "auth.invalid_credentials"
    # Nothing changed: the old password and the old refresh token still work.
    assert (await _login(client, "alice")).status_code == 200
    assert (
        await client.post(f"{AUTH}/refresh", json={"refresh_token": pair.json()["refresh_token"]})
    ).status_code == 200


async def test_change_password_policy_and_auth(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")
    pair = await _login(client, "alice")
    short = await client.post(
        f"{AUTH}/password",
        json={"current_password": PW, "new_password": "short"},
        headers=_bearer(pair),
    )
    assert short.status_code == 422
    anon = await client.post(
        f"{AUTH}/password", json={"current_password": PW, "new_password": "brand-new-pass"}
    )
    assert anon.status_code == 401


# ---------------------------------------------------------------- admin


async def test_admin_lists_users_and_looks_up_by_handle(client: httpx.AsyncClient) -> None:
    alice, bob = await _admin_and_member(client)

    listed = await client.get(USERS, headers=_bearer(alice))
    assert listed.status_code == 200
    assert [u["handle"] for u in listed.json()] == ["alice", "bob"]
    assert "password_hash" not in listed.json()[0]

    one = await client.get(USERS, params={"handle": "bob"}, headers=_bearer(alice))
    assert one.status_code == 200
    assert [u["handle"] for u in one.json()] == ["bob"]

    missing = await client.get(USERS, params={"handle": "carol"}, headers=_bearer(alice))
    assert missing.status_code == 404
    assert missing.json()["error"]["code"] == "not_found"


async def test_member_cannot_use_admin_routes(client: httpx.AsyncClient) -> None:
    alice, bob = await _admin_and_member(client)
    alice_id = (await client.get(f"{AUTH}/me", headers=_bearer(alice))).json()["id"]

    assert (await client.get(USERS, headers=_bearer(bob))).status_code == 403
    reset = await client.post(
        f"{USERS}/{alice_id}/password",
        json={"admin_password": PW, "new_password": "taken-over-now"},
        headers=_bearer(bob),
    )
    assert reset.status_code == 403
    assert reset.json()["error"]["code"] == "authz.forbidden"
    assert (await _login(client, "alice")).status_code == 200


async def test_admin_reset_signs_target_out_and_sets_password(client: httpx.AsyncClient) -> None:
    alice, bob = await _admin_and_member(client)
    bob_id = (await client.get(f"{AUTH}/me", headers=_bearer(bob))).json()["id"]

    resp = await client.post(
        f"{USERS}/{bob_id}/password",
        json={"admin_password": PW, "new_password": "temporary-pass"},
        headers=_bearer(alice),
    )
    assert resp.status_code == 204
    stale = await client.post(
        f"{AUTH}/refresh", json={"refresh_token": bob.json()["refresh_token"]}
    )
    assert stale.status_code == 401
    assert (await _login(client, "bob")).status_code == 401
    assert (await _login(client, "bob", "temporary-pass")).status_code == 200
    # The admin's own session is untouched.
    assert (
        await client.post(f"{AUTH}/refresh", json={"refresh_token": alice.json()["refresh_token"]})
    ).status_code == 200


async def test_admin_reset_refuses_self_and_unknown(client: httpx.AsyncClient) -> None:
    alice, _bob = await _admin_and_member(client)
    alice_id = (await client.get(f"{AUTH}/me", headers=_bearer(alice))).json()["id"]

    self_reset = await client.post(
        f"{USERS}/{alice_id}/password",
        json={"admin_password": PW, "new_password": "no-shortcut-here"},
        headers=_bearer(alice),
    )
    assert self_reset.status_code == 400
    assert self_reset.json()["error"]["code"] == "invalid"
    assert (await _login(client, "alice")).status_code == 200

    unknown = await client.post(
        f"{USERS}/00000000-0000-4000-8000-000000000000/password",
        json={"admin_password": PW, "new_password": "whatever-long"},
        headers=_bearer(alice),
    )
    assert unknown.status_code == 404


async def test_change_password_to_same_is_refused(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")
    pair = await _login(client, "alice")
    same = await client.post(
        f"{AUTH}/password",
        json={"current_password": PW, "new_password": PW},
        headers=_bearer(pair),
    )
    assert same.status_code == 422
    # Nothing was revoked by the refused no-op.
    assert (
        await client.post(f"{AUTH}/refresh", json={"refresh_token": pair.json()["refresh_token"]})
    ).status_code == 200


async def test_admin_reset_requires_admin_password(client: httpx.AsyncClient) -> None:
    alice, bob = await _admin_and_member(client)
    bob_id = (await client.get(f"{AUTH}/me", headers=_bearer(bob))).json()["id"]
    resp = await client.post(
        f"{USERS}/{bob_id}/password",
        json={"admin_password": "stolen-token-only", "new_password": "temporary-pass"},
        headers=_bearer(alice),
    )
    assert resp.status_code == 403
    assert resp.json()["error"]["code"] == "auth.invalid_credentials"
    assert (await _login(client, "bob")).status_code == 200


async def test_admin_cannot_reset_another_admin(client: httpx.AsyncClient) -> None:
    from sqlalchemy import update

    from app import db
    from app.models import User

    alice, bob = await _admin_and_member(client)
    # No API creates a second admin yet; seed one so the rule is enforced where
    # it is claimed rather than by the accident of there being one admin.
    async with db.get_sessionmaker()() as s:
        await s.execute(update(User).where(User.handle == "bob").values(global_role="admin"))
        await s.commit()
    bob_id = (await client.get(f"{AUTH}/me", headers=_bearer(bob))).json()["id"]
    resp = await client.post(
        f"{USERS}/{bob_id}/password",
        json={"admin_password": PW, "new_password": "taken-over-now"},
        headers=_bearer(alice),
    )
    assert resp.status_code == 403
    assert resp.json()["error"]["code"] == "authz.forbidden"
    assert (await _login(client, "bob")).status_code == 200
