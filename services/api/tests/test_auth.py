"""Local auth tests: bootstrap, authz, login, refresh rotation."""

from __future__ import annotations

import httpx

API = "/api/v1/auth"


async def _register(client: httpx.AsyncClient, handle: str, **kw: str) -> httpx.Response:
    body = {"handle": handle, "display_name": handle.title(), "password": "supersecret"}
    return await client.post(f"{API}/register", json=body, **kw)


async def test_first_user_bootstraps_as_admin(client: httpx.AsyncClient) -> None:
    resp = await _register(client, "alice")
    assert resp.status_code == 201
    assert resp.json()["global_role"] == "admin"


async def test_second_register_requires_admin(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")  # first = admin
    # anonymous second registration is forbidden
    assert (await _register(client, "bob")).status_code == 403

    # admin can create a member
    login = await client.post(f"{API}/login", json={"handle": "alice", "password": "supersecret"})
    access = login.json()["access_token"]
    created = await _register(client, "bob", headers={"Authorization": f"Bearer {access}"})
    assert created.status_code == 201
    assert created.json()["global_role"] == "member"


async def test_login_me_and_bad_password(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")
    login = await client.post(f"{API}/login", json={"handle": "alice", "password": "supersecret"})
    assert login.status_code == 200
    access = login.json()["access_token"]

    me = await client.get(f"{API}/me", headers={"Authorization": f"Bearer {access}"})
    assert me.status_code == 200
    assert me.json()["handle"] == "alice"

    bad = await client.post(f"{API}/login", json={"handle": "alice", "password": "nope"})
    assert bad.status_code == 401


async def test_refresh_rotates_and_revokes_old(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")
    login = await client.post(f"{API}/login", json={"handle": "alice", "password": "supersecret"})
    old_refresh = login.json()["refresh_token"]

    rotated = await client.post(f"{API}/refresh", json={"refresh_token": old_refresh})
    assert rotated.status_code == 200
    assert rotated.json()["refresh_token"] != old_refresh

    # the old token is now revoked
    reused = await client.post(f"{API}/refresh", json={"refresh_token": old_refresh})
    assert reused.status_code == 401


async def test_me_requires_auth(client: httpx.AsyncClient) -> None:
    assert (await client.get(f"{API}/me")).status_code in (401, 403)


async def test_duplicate_handle_conflicts(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")  # admin
    login = await client.post(f"{API}/login", json={"handle": "alice", "password": "supersecret"})
    access = login.json()["access_token"]
    dup = await _register(client, "alice", headers={"Authorization": f"Bearer {access}"})
    assert dup.status_code == 409


async def test_invalid_bearer_token_rejected(client: httpx.AsyncClient) -> None:
    resp = await client.get(f"{API}/me", headers={"Authorization": "Bearer not-a-real-token"})
    assert resp.status_code == 401


async def test_login_unknown_handle_is_401(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")
    resp = await client.post(f"{API}/login", json={"handle": "ghost", "password": "whatever-long"})
    assert resp.status_code == 401


async def test_logout_revokes_refresh_token(client: httpx.AsyncClient) -> None:
    await _register(client, "alice")
    login = await client.post(f"{API}/login", json={"handle": "alice", "password": "supersecret"})
    refresh_token = login.json()["refresh_token"]

    out = await client.post(f"{API}/logout", json={"refresh_token": refresh_token})
    assert out.status_code == 204

    reused = await client.post(f"{API}/refresh", json={"refresh_token": refresh_token})
    assert reused.status_code == 401
